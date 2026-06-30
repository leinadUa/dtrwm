mod config;
mod font;
mod layout;
mod protocol;
mod spawn;

use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::os::unix::io::BorrowedFd;
use wayland_client::{
    backend::{ObjectData, ObjectId},
    protocol::{
        wl_buffer::{self, WlBuffer},
        wl_compositor::WlCompositor,
        wl_keyboard::{self, WlKeyboard},
        wl_registry,
        wl_seat::{self, WlSeat},
        wl_shm::{self, WlShm},
        wl_shm_pool::WlShmPool,
        wl_surface::{self, WlSurface},
    },
    Connection, Dispatch, EventQueue, Proxy, QueueHandle,
};
use xkbcommon::xkb;
use std::sync::Arc;

static RELOAD_REQUESTED: AtomicBool = AtomicBool::new(false);

extern "C" fn handle_sighup(_: libc::c_int) {
    RELOAD_REQUESTED.store(true, Ordering::Release);
}
use layout::{arrange, Layout, Rect};
use protocol::{
    river_window_management_v1::client::{
        river_decoration_v1,
        river_node_v1, river_output_v1, river_pointer_binding_v1,
        river_seat_v1::{self, Modifiers},
        river_window_manager_v1,
        river_window_v1::{self, Edges},
    },
    river_xkb_bindings_v1::client::{river_xkb_binding_v1, river_xkb_bindings_v1},
    xdg_shell::client::{
        xdg_wm_base::{self, XdgWmBase},
        xdg_surface::{self, XdgSurface},
        xdg_toplevel::{self, XdgToplevel},
    },
    RiverDecorationV1, RiverNodeV1, RiverOutputV1, RiverPointerBindingV1, RiverSeatV1,
    RiverWindowManagerV1, RiverWindowV1, RiverXkbBindingV1, RiverXkbBindingsV1,
};

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

#[derive(Clone)]
enum Action {
    Spawn(String),
    Exec,
    Quit,
    Close,
    FocusNext,
    FocusPrev,
    SetLayout(Layout),
    ToggleFloat,
    ToggleFullscreen,
    Reload,
    SwitchWorkspace(u32),
    MoveToWorkspace(u32),
}

unsafe impl Send for Action {}
unsafe impl Sync for Action {}

#[derive(Clone, Copy)]
enum PointerOp {
    Move,
    Resize,
}

unsafe impl Send for PointerOp {}
unsafe impl Sync for PointerOp {}

struct ExecDialog {
    surface: WlSurface,
    xdg_surface: XdgSurface,
    xdg_toplevel: XdgToplevel,
    input: String,
    width: i32,
    height: i32,
    configured: bool,
    pool: Option<WlShmPool>,
    buffer: Option<WlBuffer>,
}

struct TopBorder {
    decor: RiverDecorationV1,
    surface: WlSurface,
    pool: Option<WlShmPool>,
    buffer: Option<WlBuffer>,
    cur_w: i32,
    cur_color: u32,
}

struct WindowState {
    proxy: RiverWindowV1,
    node: Option<RiverNodeV1>,
    top_border: Option<TopBorder>,
    actual_w: i32,
    actual_h: i32,
    floating: bool,
    floating_geom: Rect,
    workspace: u32,
    fullscreen: bool,
    is_exec_dialog: bool,
}

struct OutputState {
    _proxy: RiverOutputV1,
    x: i32,
    y: i32,
    w: i32,
    h: i32,
}

struct SeatState {
    proxy: RiverSeatV1,
    pointer_over: Option<ObjectId>,
}

struct Op {
    window_id: ObjectId,
    kind: PointerOp,
    start_geom: Rect,
    dx: i32,
    dy: i32,
}

struct State {
    wm: Option<RiverWindowManagerV1>,
    xkb: Option<RiverXkbBindingsV1>,
    compositor: Option<WlCompositor>,
    shm_global: Option<WlShm>,
    xdg_wm_base: Option<XdgWmBase>,
    wl_seat: Option<WlSeat>,
    wl_keyboard: Option<WlKeyboard>,
    xkb_ctx: xkb::Context,
    xkb_state: Option<xkb::State>,
    exec_dialog: Option<ExecDialog>,
    windows: HashMap<ObjectId, WindowState>,
    window_order: VecDeque<ObjectId>,
    outputs: HashMap<ObjectId, OutputState>,
    seats: HashMap<ObjectId, SeatState>,
    focused: Option<ObjectId>,
    focus_follows_mouse: bool,
    focus_dirty: bool,
    layouts: HashMap<u32, Layout>,
    default_layout: Layout,
    master_ratio: f64,
    border_px: i32,
    gap: i32,
    foc_color: (u32, u32, u32, u32),
    unf_color: (u32, u32, u32, u32),
    binding_defs: Vec<(u32, u32, Action)>,
    bindings_registered: bool,
    xkb_bindings: Vec<RiverXkbBindingV1>,
    move_binding: Option<RiverPointerBindingV1>,
    resize_binding: Option<RiverPointerBindingV1>,
    pending_op: Option<(ObjectId, PointerOp)>,
    current_workspace: u32,
    swap_source: Option<ObjectId>,
    swap_origin: (i32, i32),
    swap_dx: i32,
    swap_dy: i32,
    op: Option<Op>,
    op_release_pending: bool,
    pending_actions: Vec<Action>,
    running: bool,
}

fn parse_action(action: &str, arg: &str) -> Option<Action> {
    match action {
        "spawn" => Some(Action::Spawn(arg.into())),
        "exec" => Some(Action::Exec),
        "quit" => Some(Action::Quit),
        "close" => Some(Action::Close),
        "focus_next" => Some(Action::FocusNext),
        "focus_prev" => Some(Action::FocusPrev),
        "set_layout" => match arg {
            "tile" => Some(Action::SetLayout(Layout::Tile)),
            "grid" => Some(Action::SetLayout(Layout::Grid)),
            "monocle" => Some(Action::SetLayout(Layout::Monocle)),
            "float" => Some(Action::SetLayout(Layout::Float)),
            _ => {
                log::warn!("unknown layout: {arg}");
                None
            }
        },
        "toggle_float" => Some(Action::ToggleFloat),
        "toggle_fullscreen" => Some(Action::ToggleFullscreen),
        "reload" => Some(Action::Reload),
        "switch_workspace" => arg.parse().ok().map(Action::SwitchWorkspace),
        "move_to_workspace" => arg.parse().ok().map(Action::MoveToWorkspace),
        _ => {
            log::warn!("unknown action: {action}");
            None
        }
    }
}

impl State {
    fn from_config(cfg: config::Config) -> Self {
        let default_layout = match cfg.default_layout.as_str() {
            "grid" => Layout::Grid,
            "monocle" => Layout::Monocle,
            "float" => Layout::Float,
            _ => Layout::Tile,
        };

        let binding_defs: Vec<(u32, u32, Action)> = cfg
            .bindings
            .iter()
            .filter_map(|b| {
                let (keysym, mods) = config::parse_key(&b.keys)?;
                let action = parse_action(&b.action, &b.arg)?;
                Some((keysym, mods, action))
            })
            .collect();

        Self {
            wm: None,
            xkb: None,
            compositor: None,
            shm_global: None,
            xdg_wm_base: None,
            wl_seat: None,
            wl_keyboard: None,
            xkb_ctx: xkb::Context::new(xkb::CONTEXT_NO_FLAGS),
            xkb_state: None,
            exec_dialog: None,
            windows: HashMap::new(),
            window_order: VecDeque::new(),
            outputs: HashMap::new(),
            seats: HashMap::new(),
            focused: None,
            focus_follows_mouse: cfg.focus_follows_mouse,
            focus_dirty: false,
            layouts: HashMap::new(),
            default_layout,
            master_ratio: cfg.master_ratio,
            border_px: cfg.border_px,
            gap: cfg.gap,
            foc_color: config::parse_color(&cfg.colors.focused),
            unf_color: config::parse_color(&cfg.colors.unfocused),
            binding_defs,
            bindings_registered: false,
            xkb_bindings: Vec::new(),
            move_binding: None,
            resize_binding: None,
            current_workspace: 1,
            pending_op: None,
            swap_source: None,
            swap_origin: (0, 0),
            swap_dx: 0,
            swap_dy: 0,
            op: None,
            op_release_pending: false,
            pending_actions: Vec::new(),
            running: true,
        }
    }
}

fn apply_gap(rects: Vec<Rect>, gap: i32) -> Vec<Rect> {
    if gap == 0 { return rects; }
    rects.into_iter().map(|r| Rect {
        x: r.x + gap,
        y: r.y + gap,
        w: (r.w - 2 * gap).max(1),
        h: (r.h - 2 * gap).max(1),
    }).collect()
}

fn color_argb8888(r: u32, g: u32, b: u32, a: u32) -> u32 {
    let r8 = (r >> 24) as u8;
    let g8 = (g >> 24) as u8;
    let b8 = (b >> 24) as u8;
    let a8 = (a >> 24) as u8;
    ((a8 as u32) << 24) | ((r8 as u32) << 16) | ((g8 as u32) << 8) | (b8 as u32)
}

unsafe fn shm_create(size: usize) -> libc::c_int {
    use std::ffi::CString;
    static N: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
    let name = CString::new(format!("/dtrwm-{}-{}", libc::getpid(), N.fetch_add(1, Ordering::Relaxed))).unwrap();
    let fd = libc::shm_open(name.as_ptr(), libc::O_RDWR | libc::O_CREAT | libc::O_EXCL, 0o600);
    libc::shm_unlink(name.as_ptr());
    libc::ftruncate(fd, size as libc::off_t);
    fd
}

unsafe fn shm_fill(fd: libc::c_int, size: usize, color: u32) {
    let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
    if ptr != libc::MAP_FAILED {
        let pixels = std::slice::from_raw_parts_mut(ptr as *mut u32, size / 4);
        pixels.fill(color);
        libc::munmap(ptr, size);
    }
}

fn current_layout(state: &State) -> Layout {
    state.layouts.get(&state.current_workspace).copied().unwrap_or(state.default_layout)
}

fn primary_area(state: &State) -> Rect {
    state
        .outputs
        .values()
        .next()
        .map(|o| Rect { x: o.x, y: o.y, w: o.w, h: o.h })
        .unwrap_or_default()
}

fn tiled_windows(state: &State) -> Vec<ObjectId> {
    state
        .window_order
        .iter()
        .filter(|id| state.windows.get(id).map_or(false, |w| !w.floating && !w.fullscreen && w.workspace == state.current_workspace))
        .cloned()
        .collect()
}

fn cycle_focus(state: &mut State, dir: isize) {
    let order: Vec<ObjectId> = state.window_order.iter().cloned().collect();
    if order.is_empty() {
        return;
    }
    let pos = state
        .focused
        .as_ref()
        .and_then(|f| order.iter().position(|id| id == f))
        .unwrap_or(0);
    let next = ((pos as isize + dir).rem_euclid(order.len() as isize)) as usize;
    state.focused = Some(order[next].clone());
}

fn center_rect(area: Rect, w: i32, h: i32) -> Rect {
    Rect {
        x: area.x + (area.w - w) / 2,
        y: area.y + (area.h - h) / 2,
        w,
        h,
    }
}

fn register_bindings(state: &mut State, qh: &QueueHandle<State>) {
    let seat = match state.seats.values().next() {
        Some(s) => s.proxy.clone(),
        None => {
            log::warn!("register_bindings: no seat yet");
            return;
        }
    };
    let xkb = match state.xkb.clone() {
        Some(x) => x,
        None => {
            log::warn!("register_bindings: no xkb yet");
            return;
        }
    };
    log::info!("registering {} key bindings", state.binding_defs.len());

    let defs: Vec<(u32, u32, Action)> = state.binding_defs.clone();
    for (keysym, mods, action) in defs {
        log::debug!("  binding keysym=0x{:x} mods=0x{:x}", keysym, mods);
        let modifiers = Modifiers::from_bits_truncate(mods);
        let b = xkb.get_xkb_binding(&seat, keysym, modifiers, qh, action);
        b.enable();
        state.xkb_bindings.push(b);
    }

    let super_mods = Modifiers::from_bits_truncate(64);
    let mb = seat.get_pointer_binding(BTN_LEFT, super_mods, qh, PointerOp::Move);
    mb.enable();
    state.move_binding = Some(mb);

    let rb = seat.get_pointer_binding(BTN_RIGHT, super_mods, qh, PointerOp::Resize);
    rb.enable();
    state.resize_binding = Some(rb);
}

fn reload_config(state: &mut State, qh: &QueueHandle<State>) {
    let cfg = config::load();
    state.master_ratio = cfg.master_ratio;
    state.border_px = cfg.border_px;
    state.gap = cfg.gap;
    state.focus_follows_mouse = cfg.focus_follows_mouse;
    state.foc_color = config::parse_color(&cfg.colors.focused);
    state.unf_color = config::parse_color(&cfg.colors.unfocused);
    state.default_layout = match cfg.default_layout.as_str() {
        "grid" => Layout::Grid,
        "monocle" => Layout::Monocle,
        "float" => Layout::Float,
        _ => Layout::Tile,
    };

    for b in state.xkb_bindings.drain(..) {
        b.destroy();
    }
    if let Some(mb) = state.move_binding.take() { mb.destroy(); }
    if let Some(rb) = state.resize_binding.take() { rb.destroy(); }

    state.binding_defs = cfg.bindings.iter()
        .filter_map(|b| {
            let (keysym, mods) = config::parse_key(&b.keys)?;
            let action = parse_action(&b.action, &b.arg)?;
            Some((keysym, mods, action))
        })
        .collect();

    register_bindings(state, qh);
    log::info!("config reloaded");
}

fn run_manage(state: &mut State, wm: &RiverWindowManagerV1, qh: &QueueHandle<State>) {
    if RELOAD_REQUESTED.swap(false, Ordering::AcqRel) {
        reload_config(state, qh);
    }

    if !state.bindings_registered && !state.seats.is_empty() {
        register_bindings(state, qh);
        state.bindings_registered = true;
    }

    let actions: Vec<Action> = state.pending_actions.drain(..).collect();
    for action in actions {
        match action {
            Action::Close => {
                if let Some(fid) = &state.focused.clone() {
                    if let Some(win) = state.windows.get(fid) {
                        win.proxy.close();
                    }
                }
            }
            Action::FocusNext => cycle_focus(state, 1),
            Action::FocusPrev => cycle_focus(state, -1),
            Action::SetLayout(l) => { state.layouts.insert(state.current_workspace, l); }
            Action::ToggleFloat => {
                if let Some(fid) = state.focused.clone() {
                    let area = primary_area(state);
                    if let Some(win) = state.windows.get_mut(&fid) {
                        win.floating = !win.floating;
                        if win.floating {
                            let w = win.actual_w.max(300);
                            let h = win.actual_h.max(200);
                            win.floating_geom = center_rect(area, w, h);
                        }
                    }
                }
            }
            Action::ToggleFullscreen => {
                if let Some(fid) = state.focused.clone() {
                    let output = state.outputs.values().next().map(|o| o._proxy.clone());
                    if let Some(win) = state.windows.get_mut(&fid) {
                        win.fullscreen = !win.fullscreen;
                        if win.fullscreen {
                            if let Some(ref out) = output {
                                win.proxy.fullscreen(out);
                            }
                            win.proxy.inform_fullscreen();
                        } else {
                            win.proxy.exit_fullscreen();
                            win.proxy.inform_not_fullscreen();
                        }
                    }
                }
            }
            Action::SwitchWorkspace(n) => {
                state.current_workspace = n;
                state.focused = state.window_order.iter()
                    .find(|id| state.windows.get(id).map_or(false, |w| w.workspace == n))
                    .cloned();
            }
            Action::MoveToWorkspace(n) => {
                if let Some(fid) = state.focused.clone() {
                    if let Some(win) = state.windows.get_mut(&fid) {
                        win.workspace = n;
                    }
                    if n != state.current_workspace {
                        state.focused = state.window_order.iter()
                            .find(|id| **id != fid && state.windows.get(id).map_or(false, |w| w.workspace == state.current_workspace))
                            .cloned();
                    }
                }
            }
            Action::Spawn(_) | Action::Quit | Action::Reload | Action::Exec => {}
        }
    }

    if let Some((win_id, op_kind)) = state.pending_op.take() {
        let is_floating = state.windows.get(&win_id).map_or(false, |w| w.floating);

        if matches!(op_kind, PointerOp::Resize) && !is_floating {
        } else if matches!(op_kind, PointerOp::Move) && !is_floating {
            let area = primary_area(state);
            let tiled = tiled_windows(state);
            let rects = apply_gap(arrange(current_layout(state), area, tiled.len(), state.master_ratio), state.gap);
            let origin = tiled.iter().position(|id| *id == win_id)
                .and_then(|i| rects.get(i).copied())
                .map(|r| (r.x + r.w / 2, r.y + r.h / 2))
                .unwrap_or((area.x + area.w / 2, area.y + area.h / 2));
            state.swap_origin = origin;
            state.swap_dx = 0;
            state.swap_dy = 0;
            state.swap_source = Some(win_id);
            if let Some(seat) = state.seats.values().next() {
                seat.proxy.op_start_pointer();
            }
        } else {
            let start_geom = state
                .windows
                .get(&win_id)
                .map(|w| {
                    if w.floating {
                        w.floating_geom
                    } else {
                        let area = primary_area(state);
                        let tiled = tiled_windows(state);
                        let rects = apply_gap(arrange(current_layout(state), area, tiled.len(), state.master_ratio), state.gap);
                        tiled
                            .iter()
                            .position(|id| *id == win_id)
                            .and_then(|i| rects.get(i).copied())
                            .unwrap_or(area)
                    }
                })
                .unwrap_or_default();
            if let Some(win) = state.windows.get_mut(&win_id) {
                win.floating = true;
                win.floating_geom = start_geom;
            }
            state.op = Some(Op { window_id: win_id, kind: op_kind, start_geom, dx: 0, dy: 0 });
            if let Some(seat) = state.seats.values().next() {
                seat.proxy.op_start_pointer();
            }
        }
    }

    if state.op_release_pending {
        state.op_release_pending = false;
        if let Some(seat) = state.seats.values().next() {
            seat.proxy.op_end();
        }
        state.swap_source = None;
        if let Some(op) = state.op.take() {
            let geom = match op.kind {
                PointerOp::Move => Rect {
                    x: op.start_geom.x + op.dx,
                    y: op.start_geom.y + op.dy,
                    ..op.start_geom
                },
                PointerOp::Resize => Rect {
                    w: (op.start_geom.w + op.dx).max(1),
                    h: (op.start_geom.h + op.dy).max(1),
                    ..op.start_geom
                },
            };
            if let Some(win) = state.windows.get_mut(&op.window_id) {
                win.floating_geom = geom;
            }
        }
    }

    if let Some(src) = state.swap_source.clone() {
        if state.op.is_none() {
            let cursor_x = state.swap_origin.0 + state.swap_dx;
            let cursor_y = state.swap_origin.1 + state.swap_dy;
            let area = primary_area(state);
            let tiled = tiled_windows(state);
            let rects = apply_gap(arrange(current_layout(state), area, tiled.len(), state.master_ratio), state.gap);
            let target = tiled.iter().zip(rects.iter())
                .find(|(_, r)| cursor_x >= r.x && cursor_x < r.x + r.w && cursor_y >= r.y && cursor_y < r.y + r.h)
                .map(|(id, _)| id.clone());
            if let Some(tgt) = target {
                if tgt != src {
                    let src_pos = state.window_order.iter().position(|id| *id == src);
                    let dst_pos = state.window_order.iter().position(|id| *id == tgt);
                    if let (Some(sp), Some(dp)) = (src_pos, dst_pos) {
                        state.window_order.swap(sp, dp);
                    }
                }
            }
        }
    }

    let ids: Vec<ObjectId> = state.windows.keys().cloned().collect();
    for id in &ids {
        let win = state.windows.get_mut(id).unwrap();
        if win.is_exec_dialog {
            if win.node.is_none() {
                let node = win.proxy.get_node(qh, ());
                win.node = Some(node);
                win.proxy.use_ssd();
            }
            win.proxy.propose_dimensions(600, 30);
            // recompute position every manage cycle in case output dims arrived late
            {
                let area = state.outputs.values().next()
                    .map(|o| Rect { x: o.x, y: o.y, w: o.w, h: o.h })
                    .unwrap_or_default();
                if area.w > 0 {
                    if let Some(w2) = state.windows.get_mut(id) {
                        w2.floating_geom = Rect {
                            x: area.x + (area.w - 600) / 2,
                            y: area.y + 40,
                            w: 600,
                            h: 30,
                        };
                    }
                }
            }
            continue;
        }
        win.proxy.set_tiled(Edges::from_bits_truncate(15));
        if win.node.is_none() {
            let node = win.proxy.get_node(qh, ());
            win.node = Some(node);
            win.proxy.use_ssd();
            if let Some(comp) = &state.compositor {
                let surface = comp.create_surface(qh, ());
                let decor = win.proxy.get_decoration_below(&surface, qh, ());
                win.top_border = Some(TopBorder {
                    decor,
                    surface,
                    pool: None,
                    buffer: None,
                    cur_w: 0,
                    cur_color: 0,
                });
            }
        }
    }

    let area = primary_area(state);
    if area.w > 0 && area.h > 0 {
        let tiled = tiled_windows(state);
        let rects = apply_gap(arrange(current_layout(state), area, tiled.len(), state.master_ratio), state.gap);
        for (wid, rect) in tiled.iter().zip(rects.iter()) {
            if let Some(win) = state.windows.get(wid) {
                win.proxy.propose_dimensions(rect.w, rect.h);
                win.proxy.set_tiled(Edges::from_bits_truncate(15));
            }
        }

        if let Some(op) = &state.op {
            if matches!(op.kind, PointerOp::Resize) {
                let new_w = (op.start_geom.w + op.dx).max(1);
                let new_h = (op.start_geom.h + op.dy).max(1);
                if let Some(win) = state.windows.get(&op.window_id) {
                    win.proxy.propose_dimensions(new_w, new_h);
                }
            }
        }
    }

    let area = primary_area(state);
    for win in state.windows.values() {
        if win.fullscreen {
            win.proxy.propose_dimensions(area.w, area.h);
        }
    }

    let exec_dialog_id = state.windows.iter()
        .find(|(_, w)| w.is_exec_dialog)
        .map(|(id, _)| id.clone());

    if let Some(dlg_id) = exec_dialog_id {
        if let (Some(win), Some(seat)) = (state.windows.get(&dlg_id), state.seats.values().next()) {
            seat.proxy.focus_window(&win.proxy);
        }
    } else if let Some(fid) = state.focused.clone() {
        if let (Some(win), Some(seat)) =
            (state.windows.get(&fid), state.seats.values().next())
        {
            seat.proxy.focus_window(&win.proxy);
        }
    } else if let Some(seat) = state.seats.values().next() {
        seat.proxy.clear_focus();
    }

    wm.manage_finish();
}

fn update_top_border(tb: &mut TopBorder, shm: &WlShm, w: i32, h: i32, argb: u32, qh: &QueueHandle<State>) {
    if tb.cur_w != w || tb.cur_color != argb {
        if let Some(b) = tb.buffer.take() { b.destroy(); }
        if let Some(p) = tb.pool.take() { p.destroy(); }
        let size = (w * h * 4) as usize;
        if size > 0 {
            let fd = unsafe { shm_create(size) };
            if fd >= 0 {
                unsafe { shm_fill(fd, size, argb) };
                let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
                let pool = shm.create_pool(bfd, size as i32, qh, ());
                let buf = pool.create_buffer(0, w, h, w * 4, wl_shm::Format::Argb8888, qh, ());
                tb.surface.attach(Some(&buf), 0, 0);
                tb.surface.damage_buffer(0, 0, w, h);
                tb.pool = Some(pool);
                tb.buffer = Some(buf);
                tb.cur_w = w;
                tb.cur_color = argb;
                unsafe { libc::close(fd) };
            }
        }
    }
    tb.decor.set_offset(-1, -h);
    tb.decor.sync_next_commit();
    tb.surface.commit();
}

fn run_render(state: &mut State, wm: &RiverWindowManagerV1, qh: &QueueHandle<State>) {
    let border = state.border_px;
    let edges = Edges::from_bits_truncate(14);

    for win in state.windows.values() {
        if win.workspace != state.current_workspace && !win.is_exec_dialog {
            win.proxy.hide();
        }
    }

    let area = primary_area(state);
    if area.w > 0 && area.h > 0 {
        let tiled = tiled_windows(state);
        let rects = apply_gap(arrange(current_layout(state), area, tiled.len(), state.master_ratio), state.gap);

        let shm = state.shm_global.clone();
        let foc_color = state.foc_color;
        let unf_color = state.unf_color;
        let focused = state.focused.clone();
        let layout = current_layout(state);

        for (wid, rect) in tiled.iter().zip(rects.iter()) {
            let is_focused = focused.as_ref() == Some(wid);
            let (r, g, b, a) = if is_focused { foc_color } else { unf_color };
            let argb = color_argb8888(r, g, b, a);
            let win = state.windows.get_mut(wid).unwrap();

            if let Some(node) = &win.node {
                node.set_position(rect.x, rect.y);
            }
            win.proxy.set_borders(edges, border, r, g, b, a);

            if let (Some(tb), Some(ref shm)) = (&mut win.top_border, &shm) {
                update_top_border(tb, shm, rect.w + 2, border, argb, qh);
            }

            if layout == Layout::Monocle {
                if is_focused { win.proxy.show(); } else { win.proxy.hide(); }
            } else {
                win.proxy.show();
            }
        }
    }

    let wids: Vec<ObjectId> = state.window_order.iter().cloned().collect();
    let shm = state.shm_global.clone();
    for wid in &wids {
        if !state.windows.get(wid).map_or(false, |w| w.floating && !w.is_exec_dialog && w.workspace == state.current_workspace) {
            continue;
        }
        let geom = {
            let win = &state.windows[wid];
            if let Some(op) = &state.op {
                if op.window_id == *wid {
                    match op.kind {
                        PointerOp::Move => Rect { x: op.start_geom.x + op.dx, y: op.start_geom.y + op.dy, ..op.start_geom },
                        PointerOp::Resize => Rect { w: (op.start_geom.w + op.dx).max(1), h: (op.start_geom.h + op.dy).max(1), ..op.start_geom },
                    }
                } else { win.floating_geom }
            } else { win.floating_geom }
        };
        let is_focused = state.focused.as_ref() == Some(wid);
        let (r, g, b, a) = if is_focused { state.foc_color } else { state.unf_color };
        let argb = color_argb8888(r, g, b, a);
        let win = state.windows.get_mut(wid).unwrap();
        if let Some(node) = &win.node { node.set_position(geom.x, geom.y); node.place_top(); }
        win.proxy.set_borders(edges, border, r, g, b, a);
        if let (Some(tb), Some(ref shm)) = (&mut win.top_border, &shm) {
            update_top_border(tb, shm, geom.w + 2, border, argb, qh);
        }
        win.proxy.show();
    }

    for wid in &state.window_order {
        let win = &state.windows[wid];
        if !win.fullscreen || win.workspace != state.current_workspace {
            continue;
        }
        if let Some(node) = &win.node {
            node.set_position(area.x, area.y);
            node.place_top();
        }
        win.proxy.set_borders(edges, 0, 0, 0, 0, 0);
        win.proxy.show();
    }

    let exec_ids: Vec<ObjectId> = state.windows.iter()
        .filter(|(_, w)| w.is_exec_dialog)
        .map(|(id, _)| id.clone())
        .collect();
    for wid in exec_ids {
        let win = state.windows.get_mut(&wid).unwrap();
        let geom = win.floating_geom;
        if let Some(node) = &win.node {
            node.set_position(geom.x, geom.y);
            node.place_top();
        }
        win.proxy.set_borders(Edges::from_bits_truncate(0), 0, 0, 0, 0, 0);
        win.proxy.show();
    }

    wm.render_finish();
}

fn show_exec_dialog(state: &mut State, qh: &QueueHandle<State>) {
    if state.exec_dialog.is_some() { return; }
    let (comp, wm_base) = match (&state.compositor, &state.xdg_wm_base) {
        (Some(c), Some(w)) => (c.clone(), w.clone()),
        _ => { log::warn!("exec: compositor or xdg_wm_base not available"); return; }
    };
    let surface = comp.create_surface(qh, ());
    let xdg_surface = wm_base.get_xdg_surface(&surface, qh, ());
    let xdg_toplevel = xdg_surface.get_toplevel(qh, ());
    xdg_toplevel.set_app_id("dtrwm-exec".to_string());
    xdg_toplevel.set_title("exec".to_string());
    surface.commit();
    state.exec_dialog = Some(ExecDialog {
        surface,
        xdg_surface,
        xdg_toplevel,
        input: String::new(),
        width: 600,
        height: 30,
        configured: false,
        pool: None,
        buffer: None,
    });
    log::info!("exec_dialog created");
}

fn close_exec_dialog(state: &mut State) {
    if let Some(dlg) = state.exec_dialog.take() {
        if let Some(b) = dlg.buffer { b.destroy(); }
        if let Some(p) = dlg.pool { p.destroy(); }
        dlg.xdg_toplevel.destroy();
        dlg.xdg_surface.destroy();
        dlg.surface.destroy();
    }
}

fn path_completion(prefix: &str) -> Option<String> {
    if prefix.is_empty() || prefix.contains(' ') { return None; }
    let path_var = std::env::var("PATH").unwrap_or_default();
    let mut matches: Vec<String> = path_var.split(':')
        .filter_map(|dir| std::fs::read_dir(dir).ok())
        .flatten()
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().to_string_lossy().into_owned();
            if name.starts_with(prefix) { Some(name) } else { None }
        })
        .collect();
    matches.sort();
    matches.dedup();
    matches.into_iter().next()
}

fn render_exec_dialog(state: &mut State, qh: &QueueHandle<State>) {
    let shm = match &state.shm_global { Some(s) => s.clone(), None => return };
    let dlg = match &mut state.exec_dialog { Some(d) => d, None => return };
    if !dlg.configured { return; }

    let w = dlg.width.max(1) as usize;
    let h = dlg.height.max(1) as usize;
    let stride = w;
    let size = w * h * 4;

    if let Some(b) = dlg.buffer.take() { b.destroy(); }
    if let Some(p) = dlg.pool.take() { p.destroy(); }

    let fd = unsafe { shm_create(size) };
    if fd < 0 { return; }

    let buf = unsafe {
        let ptr = libc::mmap(std::ptr::null_mut(), size, libc::PROT_READ | libc::PROT_WRITE, libc::MAP_SHARED, fd, 0);
        if ptr == libc::MAP_FAILED { libc::close(fd); return; }
        std::slice::from_raw_parts_mut(ptr as *mut u32, w * h)
    };

    let bg: u32 = 0xFF1D2021;
    let fg_label: u32 = 0xFF888888;
    let fg_input: u32 = 0xFFD8DEE9;
    let fg_cursor: u32 = 0xFF88C0D0;
    let fg_hint: u32 = 0xFF555555;

    buf.fill(bg);

    let y = (h.saturating_sub(8)) / 2;
    let label = "exec: ";
    font::draw_str(buf, stride, 8, y, label, fg_label, bg);
    let input_x = 8 + label.len() * 8;
    let dlg_input = dlg.input.clone();
    font::draw_str(buf, stride, input_x, y, &dlg_input, fg_input, bg);
    let cursor_x = input_x + dlg_input.len() * 8;

    // completion hint: show the suffix of the first PATH match in dim color
    if let Some(completion) = path_completion(&dlg_input) {
        let suffix = &completion[dlg_input.len()..];
        if !suffix.is_empty() && cursor_x + suffix.len() * 8 + 8 <= w {
            font::draw_str(buf, stride, cursor_x, y, suffix, fg_hint, bg);
        }
    }

    if cursor_x + 8 <= w {
        font::draw_char(buf, stride, cursor_x, y, '_', fg_cursor, bg);
    }

    unsafe { libc::munmap(buf.as_mut_ptr() as *mut libc::c_void, size); }

    let bfd = unsafe { BorrowedFd::borrow_raw(fd) };
    let pool = shm.create_pool(bfd, size as i32, qh, ());
    let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Argb8888, qh, ());
    unsafe { libc::close(fd); }

    let dlg = state.exec_dialog.as_mut().unwrap();
    dlg.surface.attach(Some(&buffer), 0, 0);
    dlg.surface.damage_buffer(0, 0, w as i32, h as i32);
    dlg.surface.commit();
    dlg.pool = Some(pool);
    dlg.buffer = Some(buffer);
}

impl Dispatch<wl_registry::WlRegistry, ()> for State {
    fn event(
        state: &mut Self,
        registry: &wl_registry::WlRegistry,
        event: wl_registry::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        let wl_registry::Event::Global { name, interface, version } = event else { return };
        match interface.as_str() {
            "river_window_manager_v1" => {
                state.wm = Some(registry.bind(name, version.min(5), qh, ()));
            }
            "river_xkb_bindings_v1" => {
                state.xkb = Some(registry.bind(name, version.min(3), qh, ()));
            }
            "wl_compositor" => {
                state.compositor = Some(registry.bind(name, version.min(4), qh, ()));
            }
            "wl_shm" => {
                state.shm_global = Some(registry.bind(name, version.min(1), qh, ()));
            }
            "xdg_wm_base" => {
                state.xdg_wm_base = Some(registry.bind(name, version.min(5), qh, ()));
            }
            "wl_seat" => {
                if state.wl_seat.is_none() {
                    state.wl_seat = Some(registry.bind(name, version.min(7), qh, ()));
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverWindowManagerV1, ()> for State {
    fn event_created_child(opcode: u16, qh: &QueueHandle<Self>) -> Arc<dyn ObjectData> {
        match opcode {
            6 => qh.make_data::<RiverWindowV1, ()>(()),
            7 => qh.make_data::<RiverOutputV1, ()>(()),
            8 => qh.make_data::<RiverSeatV1, ()>(()),
            _ => panic!("unknown child opcode {opcode} for river_window_manager_v1"),
        }
    }

    fn event(
        state: &mut Self,
        wm: &RiverWindowManagerV1,
        event: river_window_manager_v1::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        use river_window_manager_v1::Event;
        match event {
            Event::Unavailable => {
                log::error!("river_window_manager_v1 unavailable — another WM running?");
                state.running = false;
            }
            Event::Finished => {
                log::info!("river_window_manager_v1 finished");
                state.running = false;
            }
            Event::ManageStart => {
                log::debug!("manage_start (windows={}, seats={}, outputs={})",
                    state.windows.len(), state.seats.len(), state.outputs.len());
                run_manage(state, wm, qh);
            }
            Event::RenderStart => {
                log::debug!("render_start");
                run_render(state, wm, qh);
            }
            Event::Window { id } => {
                let win_id = id.id();
                state.windows.insert(
                    win_id.clone(),
                    WindowState {
                        proxy: id,
                        node: None,
                        top_border: None,
                        actual_w: 0,
                        actual_h: 0,
                        floating: false,
                        floating_geom: Rect::default(),
                        workspace: state.current_workspace,
                        fullscreen: false,
                        is_exec_dialog: false,
                    },
                );
                state.window_order.push_back(win_id.clone());
                state.focused = Some(win_id);
            }
            Event::Output { id } => {
                log::debug!("output added");
                state.outputs.insert(
                    id.id(),
                    OutputState { _proxy: id, x: 0, y: 0, w: 0, h: 0 },
                );
            }
            Event::Seat { id } => {
                log::debug!("seat added");
                state.seats.insert(
                    id.id(),
                    SeatState { proxy: id, pointer_over: None },
                );
            }
            Event::SessionLocked | Event::SessionUnlocked => {}
        }
    }
}

impl Dispatch<RiverWindowV1, ()> for State {
    fn event(
        state: &mut Self,
        win: &RiverWindowV1,
        event: river_window_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_window_v1::Event;
        let win_id = win.id();
        match event {
            Event::Closed => {
                state.windows.remove(&win_id);
                state.window_order.retain(|id| id != &win_id);
                if state.focused.as_ref() == Some(&win_id) {
                    state.focused = state.window_order.front().cloned();
                }
            }
            Event::Dimensions { width, height } => {
                if let Some(w) = state.windows.get_mut(&win_id) {
                    w.actual_w = width;
                    w.actual_h = height;
                }
            }
            Event::FullscreenRequested { .. } => {
                if let Some(w) = state.windows.get_mut(&win_id) {
                    w.fullscreen = true;
                    w.proxy.inform_fullscreen();
                }
            }
            Event::ExitFullscreenRequested => {
                if let Some(w) = state.windows.get_mut(&win_id) {
                    w.fullscreen = false;
                    w.proxy.inform_not_fullscreen();
                }
            }
            Event::Parent { parent } => {
                if let Some(w) = state.windows.get_mut(&win_id) {
                    w.floating = parent.is_some();
                    if w.floating {
                        let area = state
                            .outputs
                            .values()
                            .next()
                            .map(|o| Rect { x: o.x, y: o.y, w: o.w, h: o.h })
                            .unwrap_or_default();
                        w.floating_geom = center_rect(area, 600, 400);
                    }
                }
            }
            Event::AppId { app_id } => {
                if app_id.as_deref() == Some("dtrwm-exec") {
                    if let Some(w) = state.windows.get_mut(&win_id) {
                        w.is_exec_dialog = true;
                        w.floating = true;
                        let area = state
                            .outputs
                            .values()
                            .next()
                            .map(|o| Rect { x: o.x, y: o.y, w: o.w, h: o.h })
                            .unwrap_or_default();
                        w.floating_geom = Rect {
                            x: area.x + (area.w - 600) / 2,
                            y: area.y + 40,
                            w: 600,
                            h: 30,
                        };
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverOutputV1, ()> for State {
    fn event(
        state: &mut Self,
        output: &RiverOutputV1,
        event: river_output_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_output_v1::Event;
        let oid = output.id();
        match event {
            Event::Position { x, y } => {
                if let Some(o) = state.outputs.get_mut(&oid) {
                    o.x = x;
                    o.y = y;
                }
            }
            Event::Dimensions { width, height } => {
                if let Some(o) = state.outputs.get_mut(&oid) {
                    o.w = width;
                    o.h = height;
                }
            }
            Event::Removed => {
                state.outputs.remove(&oid);
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverSeatV1, ()> for State {
    fn event(
        state: &mut Self,
        seat: &RiverSeatV1,
        event: river_seat_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        use river_seat_v1::Event;
        let sid = seat.id();
        match event {
            Event::PointerEnter { window } => {
                let wid = window.id();
                if let Some(s) = state.seats.get_mut(&sid) {
                    s.pointer_over = Some(wid.clone());
                }
                if state.focus_follows_mouse {
                    state.focused = Some(wid.clone());
                    state.focus_dirty = true;
                }
            }
            Event::PointerLeave => {
                if let Some(s) = state.seats.get_mut(&sid) {
                    s.pointer_over = None;
                }
            }
            Event::WindowInteraction { window } => {
                state.focused = Some(window.id());
            }
            Event::OpDelta { dx, dy } => {
                if let Some(op) = &mut state.op {
                    op.dx = dx;
                    op.dy = dy;
                } else if state.swap_source.is_some() {
                    state.swap_dx = dx;
                    state.swap_dy = dy;
                    state.focus_dirty = true;
                }
            }
            Event::OpRelease => {
                state.op_release_pending = true;
            }
            _ => {}
        }
    }
}

impl Dispatch<RiverNodeV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &RiverNodeV1,
        _: river_node_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<RiverXkbBindingsV1, ()> for State {
    fn event(
        _: &mut Self,
        _: &RiverXkbBindingsV1,
        _: river_xkb_bindings_v1::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
    }
}

impl Dispatch<RiverXkbBindingV1, Action> for State {
    fn event(
        state: &mut Self,
        _: &RiverXkbBindingV1,
        event: river_xkb_binding_v1::Event,
        action: &Action,
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if !matches!(event, river_xkb_binding_v1::Event::Pressed) {
            return;
        }
        if state.exec_dialog.is_some() { return; }
        match action {
            Action::Spawn(cmd) => {
                log::debug!("spawning: {cmd}");
                spawn::run(cmd);
            }
            Action::Exec => {
                show_exec_dialog(state, qh);
            }
            Action::Quit => {
                state.running = false;
                spawn::run("pkill river");
            }
            Action::Reload => {
                RELOAD_REQUESTED.store(true, Ordering::Release);
            }
            other => state.pending_actions.push(other.clone()),
        }
    }
}

impl Dispatch<RiverPointerBindingV1, PointerOp> for State {
    fn event(
        state: &mut Self,
        _: &RiverPointerBindingV1,
        event: river_pointer_binding_v1::Event,
        op: &PointerOp,
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            river_pointer_binding_v1::Event::Pressed => {
                let win_id = state.seats.values().find_map(|s| s.pointer_over.clone());
                log::debug!("pointer pressed op={} window={}", matches!(op, PointerOp::Move), win_id.is_some());
                if let Some(wid) = win_id {
                    state.pending_op = Some((wid, *op));
                }
            }
            river_pointer_binding_v1::Event::Released => {
                if matches!(op, PointerOp::Move) {
                    state.swap_source = None;
                }
            }
        }
    }
}

impl Dispatch<WlCompositor, ()> for State {
    fn event(_: &mut Self, _: &WlCompositor, _: wayland_client::protocol::wl_compositor::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShm, ()> for State {
    fn event(_: &mut Self, _: &WlShm, _: wl_shm::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlShmPool, ()> for State {
    fn event(_: &mut Self, _: &WlShmPool, _: wayland_client::protocol::wl_shm_pool::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlBuffer, ()> for State {
    fn event(_: &mut Self, _: &WlBuffer, _: wl_buffer::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<WlSurface, ()> for State {
    fn event(_: &mut Self, _: &WlSurface, _: wl_surface::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {}
}
impl Dispatch<RiverDecorationV1, ()> for State {
    fn event(_: &mut Self, _: &RiverDecorationV1, event: river_decoration_v1::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        match event {}
    }
}

impl Dispatch<WlSeat, ()> for State {
    fn event(
        state: &mut Self,
        seat: &WlSeat,
        event: wl_seat::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            let raw: u32 = capabilities.into();
            if raw & 2 != 0 && state.wl_keyboard.is_none() {
                state.wl_keyboard = Some(seat.get_keyboard(qh, ()));
            }
        }
    }
}

impl Dispatch<WlKeyboard, ()> for State {
    fn event(
        state: &mut Self,
        _: &WlKeyboard,
        event: wl_keyboard::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        match event {
            wl_keyboard::Event::Keymap { format, fd, size } => {
                let fmt: u32 = format.into();
                if fmt != 1 { return; }
                use std::os::unix::io::AsRawFd;
                let raw_fd = fd.as_raw_fd();
                let ptr = unsafe {
                    libc::mmap(std::ptr::null_mut(), size as usize, libc::PROT_READ, libc::MAP_PRIVATE, raw_fd, 0)
                };
                if ptr == libc::MAP_FAILED { return; }
                let s = unsafe {
                    let bytes = std::slice::from_raw_parts(ptr as *const u8, size as usize);
                    std::str::from_utf8(bytes).unwrap_or("").trim_end_matches('\0').to_string()
                };
                unsafe { libc::munmap(ptr, size as usize); }
                if let Some(km) = xkb::Keymap::new_from_string(&state.xkb_ctx, s, xkb::KEYMAP_FORMAT_TEXT_V1, xkb::COMPILE_NO_FLAGS) {
                    state.xkb_state = Some(xkb::State::new(&km));
                }
            }
            wl_keyboard::Event::Modifiers { mods_depressed, mods_latched, mods_locked, group, .. } => {
                if let Some(xs) = &mut state.xkb_state {
                    xs.update_mask(mods_depressed, mods_latched, mods_locked, 0, 0, group);
                }
            }
            wl_keyboard::Event::Key { key, state: key_state, .. } => {
                let pressed: u32 = key_state.into();
                if pressed == 0 { return; }
                if state.exec_dialog.is_none() { return; }

                let (sym, utf8) = if let Some(xs) = &state.xkb_state {
                    let kc = key + 8;
                    (xs.key_get_one_sym(kc.into()).raw(), xs.key_get_utf8(kc.into()))
                } else { return };

                match sym {
                    0xff0d | 0xff8d => {
                        let cmd = state.exec_dialog.as_ref().unwrap().input.clone();
                        close_exec_dialog(state);
                        if !cmd.is_empty() { spawn::run(&cmd); }
                    }
                    0xff1b => { close_exec_dialog(state); }
                    0xff08 => {
                        if let Some(dlg) = &mut state.exec_dialog { dlg.input.pop(); }
                        render_exec_dialog(state, qh);
                    }
                    0xff09 => {
                        let current = state.exec_dialog.as_ref().map(|d| d.input.clone()).unwrap_or_default();
                        if let Some(completed) = path_completion(&current) {
                            if let Some(dlg) = &mut state.exec_dialog {
                                dlg.input = completed;
                            }
                            render_exec_dialog(state, qh);
                        }
                    }
                    _ => {
                        if !utf8.is_empty() && utf8.chars().all(|c| !c.is_control()) {
                            if let Some(dlg) = &mut state.exec_dialog { dlg.input.push_str(&utf8); }
                            render_exec_dialog(state, qh);
                        }
                    }
                }
            }
            _ => {}
        }
    }
}

impl Dispatch<XdgWmBase, ()> for State {
    fn event(
        _: &mut Self,
        wm_base: &XdgWmBase,
        event: xdg_wm_base::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<XdgSurface, ()> for State {
    fn event(
        state: &mut Self,
        xdg_surface: &XdgSurface,
        event: xdg_surface::Event,
        _: &(),
        _: &Connection,
        qh: &QueueHandle<Self>,
    ) {
        if let xdg_surface::Event::Configure { serial } = event {
            xdg_surface.ack_configure(serial);
            if let Some(dlg) = &mut state.exec_dialog {
                dlg.configured = true;
            }
            render_exec_dialog(state, qh);
        }
    }
}

impl Dispatch<XdgToplevel, ()> for State {
    fn event(
        state: &mut Self,
        _: &XdgToplevel,
        event: xdg_toplevel::Event,
        _: &(),
        _: &Connection,
        _: &QueueHandle<Self>,
    ) {
        match event {
            xdg_toplevel::Event::Configure { width, height, .. } => {
                if let Some(dlg) = &mut state.exec_dialog {
                    if width > 0 { dlg.width = width; }
                    if height > 0 { dlg.height = height; }
                }
            }
            xdg_toplevel::Event::Close => {
                close_exec_dialog(state);
            }
            _ => {}
        }
    }
}

struct Tee {
    file: std::fs::File,
}

impl std::io::Write for Tee {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        let _ = std::io::stderr().write(buf);
        self.file.write(buf)
    }
    fn flush(&mut self) -> std::io::Result<()> {
        let _ = std::io::stderr().flush();
        self.file.flush()
    }
}

fn init_logging() {
    let xdg = xdg::BaseDirectories::with_prefix("dtrwm").unwrap();
    let log_path = xdg.place_data_file("dtrwm.log").unwrap();

    let mut builder = env_logger::Builder::from_default_env();
    builder.filter_level(log::LevelFilter::Debug);

    match std::fs::OpenOptions::new().create(true).append(true).open(log_path) {
        Ok(file) => builder.target(env_logger::Target::Pipe(Box::new(Tee { file }))),
        Err(_) => builder.target(env_logger::Target::Stderr),
    };

    builder.init();
}

fn main() {
    init_logging();
    log::info!("dtrwm starting");

    let cfg = config::load();
    let conn = match Connection::connect_to_env() {
        Ok(c) => c,
        Err(e) => {
            log::error!("failed to connect to Wayland display: {e}");
            eprintln!("failed to connect to Wayland display: {e}");
            std::process::exit(1);
        }
    };
    let mut queue: EventQueue<State> = conn.new_event_queue();
    let qh = queue.handle();

    conn.display().get_registry(&qh, ());

    let mut state = State::from_config(cfg);
    if let Err(e) = queue.roundtrip(&mut state) {
        log::error!("initial roundtrip failed: {e}");
        std::process::exit(1);
    }

    if state.wm.is_none() {
        log::error!("river_window_manager_v1 not available — is River 0.4+ running?");
        std::process::exit(1);
    }

    unsafe { libc::signal(libc::SIGHUP, handle_sighup as *const () as libc::sighandler_t); }

    log::info!("dtrwm started");

    state.wm.as_ref().unwrap().manage_dirty();
    if let Err(e) = queue.flush() {
        log::error!("flush error: {e}");
    }

    while state.running {
        if RELOAD_REQUESTED.load(Ordering::Relaxed) || state.focus_dirty {
            state.focus_dirty = false;
            if let Some(wm) = &state.wm {
                wm.manage_dirty();
            }
            queue.flush().ok();
        }

        match queue.blocking_dispatch(&mut state) {
            Ok(_) => {}
            Err(e) => {
                let s = e.to_string();
                if s.contains("Interrupted") || s.contains("EINTR") {
                    continue;
                }
                log::error!("dispatch error: {e}");
                break;
            }
        }
    }
}
