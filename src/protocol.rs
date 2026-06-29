#![allow(dead_code, unused_imports, clippy::too_many_arguments, non_camel_case_types)]

use wayland_client;

pub mod river_window_management_v1 {
    use wayland_client;
    use wayland_client::protocol::__interfaces::*;

    wayland_scanner::generate_interfaces!("protocols/river-window-management-v1.xml");

    pub mod client {
        use super::*;
        use wayland_client;
        use wayland_client::protocol::wl_output;
        use wayland_client::protocol::wl_seat;
        use wayland_client::protocol::wl_surface;

        wayland_scanner::generate_client_code!("protocols/river-window-management-v1.xml");
    }
}

pub mod river_xkb_bindings_v1 {
    use super::river_window_management_v1::*;
    use wayland_client;

    wayland_scanner::generate_interfaces!("protocols/river-xkb-bindings-v1.xml");

    pub mod client {
        use super::*;
        use crate::protocol::river_window_management_v1::client::*;
        use wayland_client;

        wayland_scanner::generate_client_code!("protocols/river-xkb-bindings-v1.xml");
    }
}

pub use river_window_management_v1::client::river_node_v1::RiverNodeV1;
pub use river_window_management_v1::client::river_output_v1::RiverOutputV1;
pub use river_window_management_v1::client::river_pointer_binding_v1::RiverPointerBindingV1;
pub use river_window_management_v1::client::river_seat_v1::RiverSeatV1;
pub use river_window_management_v1::client::river_window_manager_v1::RiverWindowManagerV1;
pub use river_window_management_v1::client::river_window_v1::RiverWindowV1;
pub use river_xkb_bindings_v1::client::river_xkb_binding_v1::RiverXkbBindingV1;
pub use river_xkb_bindings_v1::client::river_xkb_bindings_v1::RiverXkbBindingsV1;
