use std::fs::File;
use std::os::unix::prelude::AsFd;

use calloop::EventLoop;
use calloop_wayland_source::WaylandSource;
use wayland_client::{
    globals::{registry_queue_init, Global, GlobalList, GlobalListHandler},
    protocol::{wl_buffer, wl_compositor, wl_keyboard, wl_seat, wl_shm, wl_surface},
    Connection, Dispatch, NoopIgnore, QueueHandle,
};

use wayland_protocols::xdg::shell::client::{xdg_surface, xdg_toplevel, xdg_wm_base};

struct GlobalData;

fn main() {
    let conn = Connection::connect_to_env().unwrap();

    let (globals, event_queue) = registry_queue_init(&conn).unwrap();
    let qh = event_queue.handle();

    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().expect("Failed to initialize the event loop!");
    let loop_handle = event_loop.handle();

    WaylandSource::new(conn, event_queue).insert(&loop_handle).unwrap();
    let stop_handle = event_loop.get_signal();

    let mut state = State::new(globals, qh, stop_handle);

    println!("Starting the example window app, press <ESC> to quit.");

    event_loop.run(None, &mut state, |_| {}).unwrap();
}

struct State {
    base_surface: wl_surface::WlSurface,
    buffer: wl_buffer::WlBuffer,
    configured: bool,
    stop_handle: calloop::LoopSignal,
}

impl State {
    fn new(globals: GlobalList, qh: QueueHandle<Self>, stop_handle: calloop::LoopSignal) -> Self {
        let wm_base =
            globals.bind_singleton::<xdg_wm_base::XdgWmBase, _, _>(1..=1, &qh, GlobalData).unwrap();
        let compositor = globals
            .bind_singleton::<wl_compositor::WlCompositor, _, _>(1..=1, &qh, NoopIgnore)
            .unwrap();

        let base_surface = compositor.create_surface(&qh, NoopIgnore);
        let xdg_surface = wm_base.get_xdg_surface(&base_surface, &qh, GlobalData);
        let xdg_toplevel = xdg_surface.get_toplevel(&qh, GlobalData);
        xdg_toplevel.set_title("A fantastic window!".into());
        base_surface.commit();

        for global in globals.contents().clone_list() {
            if global.interface == "wl_seat" {
                globals
                    .bind_specific::<wl_seat::WlSeat, _, _>(global.name, 1..=1, &qh, GlobalData)
                    .unwrap();
            }
        }

        let shm = globals.bind_singleton::<wl_shm::WlShm, _, _>(1..=1, &qh, NoopIgnore).unwrap();

        let (init_w, init_h) = (320, 240);

        let mut file = tempfile::tempfile().unwrap();
        draw(&mut file, (init_w, init_h));
        let pool = shm.create_pool(file.as_fd(), (init_w * init_h * 4) as i32, &qh, NoopIgnore);
        let buffer = pool.create_buffer(
            0,
            init_w as i32,
            init_h as i32,
            (init_w * 4) as i32,
            wl_shm::Format::Argb8888,
            &qh,
            NoopIgnore,
        );

        State { base_surface, buffer, configured: false, stop_handle }
    }
}

impl GlobalListHandler for State {
    fn runtime_add_global(
        &mut self,
        globals: &GlobalList,
        _conn: &Connection,
        qh: &QueueHandle<Self>,
        global: &Global,
    ) {
        if global.interface == "wl_seat" {
            globals
                .bind_specific::<wl_seat::WlSeat, _, _>(global.name, 1..=1, qh, GlobalData)
                .unwrap();
        }
    }
}

fn draw(tmp: &mut File, (buf_x, buf_y): (u32, u32)) {
    use std::cmp::min;
    use std::io::Write;
    let mut buf = std::io::BufWriter::new(tmp);
    for y in 0..buf_y {
        for x in 0..buf_x {
            let a = 0xFF;
            let r = min(((buf_x - x) * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
            let g = min((x * 0xFF) / buf_x, ((buf_y - y) * 0xFF) / buf_y);
            let b = min(((buf_x - x) * 0xFF) / buf_x, (y * 0xFF) / buf_y);

            let color = (a << 24) + (r << 16) + (g << 8) + b;
            buf.write_all(&color.to_ne_bytes()).unwrap();
        }
    }
    buf.flush().unwrap();
}

impl Dispatch<xdg_wm_base::XdgWmBase, State> for GlobalData {
    fn event(
        &self,
        _: &mut State,
        wm_base: &xdg_wm_base::XdgWmBase,
        event: xdg_wm_base::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let xdg_wm_base::Event::Ping { serial } = event {
            wm_base.pong(serial);
        }
    }
}

impl Dispatch<xdg_surface::XdgSurface, State> for GlobalData {
    fn event(
        &self,
        state: &mut State,
        xdg_surface: &xdg_surface::XdgSurface,
        event: xdg_surface::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let xdg_surface::Event::Configure { serial, .. } = event {
            xdg_surface.ack_configure(serial);
            state.base_surface.attach(Some(&state.buffer), 0, 0);
            state.base_surface.commit();
            state.configured = true;
        }
    }
}

impl Dispatch<xdg_toplevel::XdgToplevel, State> for GlobalData {
    fn event(
        &self,
        state: &mut State,
        _: &xdg_toplevel::XdgToplevel,
        event: xdg_toplevel::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let xdg_toplevel::Event::Close = event {
            state.stop_handle.stop();
        }
    }
}

impl Dispatch<wl_seat::WlSeat, State> for GlobalData {
    fn event(
        &self,
        _: &mut State,
        seat: &wl_seat::WlSeat,
        event: wl_seat::Event,
        _: &Connection,
        qh: &QueueHandle<State>,
    ) {
        if let wl_seat::Event::Capabilities { capabilities } = event {
            if capabilities.contains(wl_seat::Capability::Keyboard) {
                seat.get_keyboard(qh, GlobalData);
            }
        }
    }
}

impl Dispatch<wl_keyboard::WlKeyboard, State> for GlobalData {
    fn event(
        &self,
        state: &mut State,
        _: &wl_keyboard::WlKeyboard,
        event: <wl_keyboard::WlKeyboard as wayland_client::Proxy>::Event,
        _: &Connection,
        _: &QueueHandle<State>,
    ) {
        if let wl_keyboard::Event::Key { key, .. } = event {
            if key == 1 {
                // ESC key
                state.stop_handle.stop();
            }
        }
    }
}
