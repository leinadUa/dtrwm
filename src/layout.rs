use std::cmp;

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum Layout {
    #[default]
    Tile,
    Grid,
    Monocle,
    Float,
}

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

pub fn arrange(layout: Layout, area: Rect, n: usize, master_ratio: f64) -> Vec<Rect> {
    if n == 0 {
        return vec![];
    }
    match layout {
        Layout::Tile => tile(area, n, master_ratio),
        Layout::Grid => grid(area, n),
        Layout::Monocle | Layout::Float => vec![area; n],
    }
}

fn tile(area: Rect, n: usize, ratio: f64) -> Vec<Rect> {
    if n == 1 {
        return vec![area];
    }
    let master_w = (area.w as f64 * ratio) as i32;
    let stack_w = area.w - master_w;
    let stack_n = (n - 1) as i32;
    let base_h = area.h / stack_n;
    let extra = area.h % stack_n;

    let mut out = Vec::with_capacity(n);
    out.push(Rect { x: area.x, y: area.y, w: master_w, h: area.h });

    let mut y = area.y;
    for i in 0..stack_n as usize {
        let h = base_h + if (i as i32) < extra { 1 } else { 0 };
        out.push(Rect { x: area.x + master_w, y, w: stack_w, h });
        y += h;
    }
    out
}

fn grid(area: Rect, n: usize) -> Vec<Rect> {
    if n == 1 {
        return vec![area];
    }
    let cols = cmp::max(1, (n as f64).sqrt().ceil() as usize);
    let rows = n.div_ceil(cols);
    let _base_cw = area.w / cols as i32;
    let base_ch = area.h / rows as i32;

    let mut out = Vec::with_capacity(n);
    for i in 0..n {
        let row = i / cols;
        let col = i % cols;
        let is_last_row = row == rows - 1;
        let items_in_row = if is_last_row { n - row * cols } else { cols };

        let cell_w = area.w / items_in_row as i32;
        let x = area.x + col as i32 * cell_w;
        let y = area.y + row as i32 * base_ch;
        let h = if is_last_row { area.h - row as i32 * base_ch } else { base_ch };
        out.push(Rect { x, y, w: cell_w, h });
    }
    out
}
