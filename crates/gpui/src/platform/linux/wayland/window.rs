use std::{
    cell::{Ref, RefCell, RefMut},
    ffi::c_void,
    ptr::NonNull,
    rc::Rc,
    sync::Arc,
};

use bitflags::bitflags;

use blade_graphics as gpu;
use collections::HashMap;
use futures::channel::oneshot::Receiver;

use raw_window_handle as rwh;
use wayland_backend::client::ObjectId;
use wayland_client::WEnum;
use wayland_client::{protocol::wl_surface, Proxy};
use wayland_protocols::xdg::shell::client::xdg_surface;
use wayland_protocols::xdg::shell::client::xdg_toplevel::{self};
use wayland_protocols::xdg::{
    decoration::zv1::client::zxdg_toplevel_decoration_v1::{self, ZxdgToplevelDecorationV1},
    shell::client::xdg_toplevel::XdgToplevel,
};
use wayland_protocols::{
    wp::fractional_scale::v1::client::wp_fractional_scale_v1,
    xdg::shell::client::xdg_surface::XdgSurface,
};
use wayland_protocols::{
    wp::viewporter::client::wp_viewport, xdg::shell::client::xdg_popup::XdgPopup,
};
use wayland_protocols_plasma::blur::client::org_kde_kwin_blur;
use wayland_protocols_wlr::layer_shell::v1::client::{
    zwlr_layer_shell_v1,
    zwlr_layer_surface_v1::{self, ZwlrLayerSurfaceV1},
};

use crate::scene::Scene;
use crate::{
    platform::{
        blade::{BladeContext, BladeRenderer, BladeSurfaceConfig},
        linux::wayland::{display::WaylandDisplay, serial::SerialKind},
        PlatformAtlas, PlatformInputHandler, PlatformWindow,
    },
    WindowKind,
};
use crate::{
    px, size, AnyWindowHandle, Bounds, Decorations, Globals, GpuSpecs, Modifiers, Output, Pixels,
    PlatformDisplay, PlatformInput, Point, PromptLevel, RequestFrameOptions, ResizeEdge,
    ScaledPixels, Size, Tiling, WaylandClientStatePtr, WindowAppearance,
    WindowBackgroundAppearance, WindowBounds, WindowControls, WindowDecorations, WindowParams,
};

#[derive(Default)]
pub(crate) struct Callbacks {
    request_frame: Option<Box<dyn FnMut(RequestFrameOptions)>>,
    input: Option<Box<dyn FnMut(crate::PlatformInput) -> crate::DispatchEventResult>>,
    active_status_change: Option<Box<dyn FnMut(bool)>>,
    hover_status_change: Option<Box<dyn FnMut(bool)>>,
    resize: Option<Box<dyn FnMut(Size<Pixels>, f32)>>,
    moved: Option<Box<dyn FnMut()>>,
    should_close: Option<Box<dyn FnMut() -> bool>>,
    close: Option<Box<dyn FnOnce()>>,
    appearance_changed: Option<Box<dyn FnMut()>>,
}

struct RawWindow {
    window: *mut c_void,
    display: *mut c_void,
}

impl rwh::HasWindowHandle for RawWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        let window = NonNull::new(self.window).unwrap();
        let handle = rwh::WaylandWindowHandle::new(window);
        Ok(unsafe { rwh::WindowHandle::borrow_raw(handle.into()) })
    }
}
impl rwh::HasDisplayHandle for RawWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        let display = NonNull::new(self.display).unwrap();
        let handle = rwh::WaylandDisplayHandle::new(display);
        Ok(unsafe { rwh::DisplayHandle::borrow_raw(handle.into()) })
    }
}

#[derive(Debug)]
struct InProgressConfigure {
    size: Option<Size<Pixels>>,
    fullscreen: bool,
    maximized: bool,
    tiling: Tiling,
}

/// The z-depth of a layer
///
/// These values indicate which order in which layer surfaces are rendered.
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum Layer {
    /// The background layer
    Background,
    /// The bottom layer
    Bottom,
    /// The top layer
    Top,
    /// The overlay layer
    Overlay,
}

bitflags! {
    /// The anchor point for a layer shell surface
    #[derive(Copy, Clone, Debug, PartialEq, Eq)]
    pub struct Anchor: u32 {
        /// The top edge of the surface
        const TOP = 1;
        /// The bottom edge of the surface
        const BOTTOM = 2;
        /// The left edge of the surface
        const LEFT = 4;
        /// The right edge of the surface
        const RIGHT = 8;
    }
}

/// Types of keyboard interaction possible for a layer shell surface
#[derive(Copy, Clone, Debug, PartialEq, Eq)]
pub enum KeyboardInteractivity {
    /// No keyboard focus is possible
    None,
    ///Request exclusive keyboard focus
    Exclusive,
    /// Request regular keyboard focus semantics
    OnDemand,
}

/// Settings for a layer shell surface
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LayerShellSettings {
    /// Layer of the surface
    pub layer: Layer,
    /// Anchor point of the surface
    pub anchor: Anchor,
    /// The exclusive edge will prevent other surfaces from being placed in the same area
    pub exclusive_zone: Option<Pixels>,
    /// The distance away from the anchor point
    pub margin: Option<(Pixels, Pixels, Pixels, Pixels)>,
    /// Types of keyboard interaction possible for layer shell surfaces
    pub keyboard_interactivity: KeyboardInteractivity,
    /// Whether the surface should receive pointer events
    pub pointer_interactivity: bool,
    /// Namespace for the layer shell surface
    pub namespace: String,
}

impl Default for LayerShellSettings {
    fn default() -> Self {
        Self {
            layer: Layer::Top,
            anchor: Anchor::RIGHT | Anchor::LEFT,
            exclusive_zone: None,
            margin: None,
            keyboard_interactivity: KeyboardInteractivity::Exclusive,
            pointer_interactivity: true,
            namespace: String::new(),
        }
    }
}

impl From<Layer> for zwlr_layer_shell_v1::Layer {
    fn from(layer: Layer) -> Self {
        match layer {
            Layer::Background => Self::Background,
            Layer::Bottom => Self::Bottom,
            Layer::Top => Self::Top,
            Layer::Overlay => Self::Overlay,
        }
    }
}

impl From<KeyboardInteractivity> for zwlr_layer_surface_v1::KeyboardInteractivity {
    fn from(interactivity: KeyboardInteractivity) -> Self {
        match interactivity {
            KeyboardInteractivity::None => Self::None,
            KeyboardInteractivity::Exclusive => Self::Exclusive,
            KeyboardInteractivity::OnDemand => Self::OnDemand,
        }
    }
}

enum Surface {
    Xdg((XdgSurface, XdgToplevel, Option<ZxdgToplevelDecorationV1>)),
    Layer(ZwlrLayerSurfaceV1),
    Popup((XdgPopup, XdgSurface)),
}

impl Surface {
    fn xdg(&self) -> Option<&XdgSurface> {
        match self {
            Surface::Xdg((surface, _, _)) => Some(surface),
            _ => None,
        }
    }

    fn toplevel(&self) -> Option<&XdgToplevel> {
        match self {
            Surface::Xdg((_, toplevel, _)) => Some(toplevel),
            _ => None,
        }
    }

    fn decoration(&self) -> Option<&ZxdgToplevelDecorationV1> {
        match self {
            Surface::Xdg((_, _, decoration)) => decoration.as_ref(),
            _ => None,
        }
    }

    fn layer(&self) -> Option<&ZwlrLayerSurfaceV1> {
        match self {
            Surface::Layer(surface) => Some(surface),
            _ => None,
        }
    }

    fn popop(&self) {
        unimplemented!()
    }

    fn destory(&self) {
        match self {
            Surface::Xdg((surface, toplevel, decoration)) => {
                surface.destroy();
                toplevel.destroy();
                if let Some(decoration) = decoration {
                    decoration.destroy();
                }
            }
            Surface::Layer(layer_shell) => layer_shell.destroy(),
            Surface::Popup(_) => {
                unimplemented!()
            }
        }
    }
}

struct WaylandWindowState {
    acknowledged_first_configure: bool,
    pub wl_surface: wl_surface::WlSurface,
    surface: Surface,
    app_id: Option<String>,
    appearance: WindowAppearance,
    blur: Option<org_kde_kwin_blur::OrgKdeKwinBlur>,
    viewport: Option<wp_viewport::WpViewport>,
    outputs: HashMap<ObjectId, Output>,
    display: Option<(ObjectId, Output)>,
    globals: Globals,
    renderer: BladeRenderer,
    bounds: Bounds<Pixels>,
    scale: f32,
    input_handler: Option<PlatformInputHandler>,
    decorations: WindowDecorations,
    background_appearance: WindowBackgroundAppearance,
    fullscreen: bool,
    maximized: bool,
    tiling: Tiling,
    window_bounds: Bounds<Pixels>,
    client: WaylandClientStatePtr,
    handle: AnyWindowHandle,
    active: bool,
    hovered: bool,
    in_progress_configure: Option<InProgressConfigure>,
    in_progress_window_controls: Option<WindowControls>,
    window_controls: WindowControls,
    inset: Option<Pixels>,
}

#[derive(Clone)]
pub(crate) struct WaylandWindowStatePtr {
    state: Rc<RefCell<WaylandWindowState>>,
    callbacks: Rc<RefCell<Callbacks>>,
}

impl WaylandWindowState {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn new(
        handle: AnyWindowHandle,
        wl_surface: wl_surface::WlSurface,
        surface: Surface,
        appearance: WindowAppearance,
        viewport: Option<wp_viewport::WpViewport>,
        client: WaylandClientStatePtr,
        globals: Globals,
        gpu_context: &BladeContext,
        options: WindowParams,
    ) -> anyhow::Result<Self> {
        let renderer = {
            let raw_window = RawWindow {
                window: wl_surface.id().as_ptr().cast::<c_void>(),
                display: wl_surface
                    .backend()
                    .upgrade()
                    .unwrap()
                    .display_ptr()
                    .cast::<c_void>(),
            };
            let config = BladeSurfaceConfig {
                size: gpu::Extent {
                    width: options.bounds.size.width.0 as u32,
                    height: options.bounds.size.height.0 as u32,
                    depth: 1,
                },
                transparent: true,
            };
            BladeRenderer::new(gpu_context, &raw_window, config)?
        };

        Ok(Self {
            acknowledged_first_configure: false,
            wl_surface,
            surface,
            app_id: None,
            blur: None,
            viewport,
            globals,
            outputs: HashMap::default(),
            display: None,
            renderer,
            bounds: options.bounds,
            scale: 1.0,
            input_handler: None,
            decorations: WindowDecorations::Client,
            background_appearance: WindowBackgroundAppearance::Opaque,
            fullscreen: false,
            maximized: false,
            tiling: Tiling::default(),
            window_bounds: options.bounds,
            in_progress_configure: None,
            client,
            appearance,
            handle,
            active: false,
            hovered: false,
            in_progress_window_controls: None,
            window_controls: WindowControls::default(),
            inset: None,
        })
    }

    pub fn is_transparent(&self) -> bool {
        self.decorations == WindowDecorations::Client
            || self.background_appearance != WindowBackgroundAppearance::Opaque
    }

    pub fn primary_output_scale(&mut self) -> i32 {
        let mut scale = 1;
        let mut current_output = self.display.take();
        for (id, output) in self.outputs.iter() {
            if let Some((_, output_data)) = &current_output {
                if output.scale > output_data.scale {
                    current_output = Some((id.clone(), output.clone()));
                }
            } else {
                current_output = Some((id.clone(), output.clone()));
            }
            scale = scale.max(output.scale);
        }
        self.display = current_output;
        scale
    }
}

pub(crate) struct WaylandWindow(pub WaylandWindowStatePtr);
pub(crate) enum ImeInput {
    InsertText(String),
    SetMarkedText(String),
    UnmarkText,
    DeleteText,
}

impl Drop for WaylandWindow {
    fn drop(&mut self) {
        let mut state = self.0.state.borrow_mut();
        let surface_id = state.wl_surface.id();
        let client = state.client.clone();

        state.renderer.destroy();
        if let Some(blur) = &state.blur {
            blur.release();
        }
        if let Some(viewport) = &state.viewport {
            viewport.destroy();
        }
        state.wl_surface.destroy();
        state.surface.destory();

        let state_ptr = self.0.clone();
        state
            .globals
            .executor
            .spawn(async move {
                state_ptr.close();
                client.drop_window(&surface_id)
            })
            .detach();
        drop(state);
    }
}

impl WaylandWindow {
    fn borrow(&self) -> Ref<WaylandWindowState> {
        self.0.state.borrow()
    }

    fn borrow_mut(&self) -> RefMut<WaylandWindowState> {
        self.0.state.borrow_mut()
    }

    pub fn new(
        handle: AnyWindowHandle,
        globals: Globals,
        gpu_context: &BladeContext,
        client: WaylandClientStatePtr,
        params: WindowParams,
        appearance: WindowAppearance,
    ) -> anyhow::Result<(Self, ObjectId)> {
        let wl_surface = globals.compositor.create_surface(&globals.qh, ());

        let surface = match params.kind {
            WindowKind::Normal => {
                let xdg_surface =
                    globals
                        .wm_base
                        .get_xdg_surface(&wl_surface, &globals.qh, wl_surface.id());
                let toplevel = xdg_surface.get_toplevel(&globals.qh, wl_surface.id());

                if let Some(size) = params.window_min_size {
                    toplevel.set_min_size(size.width.0 as i32, size.height.0 as i32);
                }

                // Attempt to set up window decorations based on the requested configuration
                let decoration = globals
                    .decoration_manager
                    .as_ref()
                    .map(|decoration_manager| {
                        decoration_manager.get_toplevel_decoration(
                            &toplevel,
                            &globals.qh,
                            wl_surface.id(),
                        )
                    });

                Surface::Xdg((xdg_surface, toplevel, decoration))
            }
            WindowKind::LayerShell(ref layer_shell_settings) => {
                let layer_surface = globals.layer_shell.get_layer_surface(
                    &wl_surface,
                    None,
                    layer_shell_settings.layer.into(),
                    layer_shell_settings.namespace.clone(),
                    &globals.qh,
                    wl_surface.id(),
                );
                layer_surface.set_anchor(zwlr_layer_surface_v1::Anchor::from_bits_truncate(
                    layer_shell_settings.anchor.bits(),
                ));
                layer_surface.set_size(
                    params.bounds.size.width.0 as u32,
                    params.bounds.size.height.0 as u32,
                );
                layer_surface
                    .set_keyboard_interactivity(layer_shell_settings.keyboard_interactivity.into());
                if !layer_shell_settings.pointer_interactivity {
                    let region = globals.compositor.create_region(&globals.qh, ());
                    wl_surface.set_input_region(Some(&region));
                    region.destroy();
                }
                if let Some(margin) = layer_shell_settings.margin {
                    layer_surface.set_margin(
                        margin.0 .0 as i32,
                        margin.1 .0 as i32,
                        margin.2 .0 as i32,
                        margin.3 .0 as i32,
                    );
                }
                if let Some(exclusive_zone) = layer_shell_settings.exclusive_zone {
                    layer_surface.set_exclusive_zone(exclusive_zone.0 as i32);
                }

                Surface::Layer(layer_surface)
            }
            WindowKind::PopUp => {
                unimplemented!()
            }
        };

        if let Some(fractional_scale_manager) = globals.fractional_scale_manager.as_ref() {
            fractional_scale_manager.get_fractional_scale(
                &wl_surface,
                &globals.qh,
                wl_surface.id(),
            );
        }

        let viewport = globals
            .viewporter
            .as_ref()
            .map(|viewporter| viewporter.get_viewport(&wl_surface, &globals.qh, ()));

        let this = Self(WaylandWindowStatePtr {
            state: Rc::new(RefCell::new(WaylandWindowState::new(
                handle,
                wl_surface.clone(),
                surface,
                appearance,
                viewport,
                client,
                globals,
                gpu_context,
                params,
            )?)),
            callbacks: Rc::new(RefCell::new(Callbacks::default())),
        });

        // Kick things off
        wl_surface.commit();

        Ok((this, wl_surface.id()))
    }
}

impl WaylandWindowStatePtr {
    pub fn handle(&self) -> AnyWindowHandle {
        self.state.borrow().handle
    }

    pub fn surface(&self) -> wl_surface::WlSurface {
        self.state.borrow().wl_surface.clone()
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.state, &other.state)
    }

    pub fn frame(&self) {
        let mut state = self.state.borrow_mut();
        state
            .wl_surface
            .frame(&state.globals.qh, state.wl_surface.id());
        drop(state);

        let mut cb = self.callbacks.borrow_mut();
        if let Some(fun) = cb.request_frame.as_mut() {
            fun(Default::default());
        }
    }

    pub fn handle_xdg_surface_event(&self, event: xdg_surface::Event) {
        let mut state = self.state.borrow_mut();
        if state.surface.xdg().is_none() {
            log::error!("xdg_surface is missing");
            return;
        }
        match event {
            xdg_surface::Event::Configure { serial } => {
                drop(state);
                {
                    let mut state = self.state.borrow_mut();
                    if let Some(window_controls) = state.in_progress_window_controls.take() {
                        state.window_controls = window_controls;

                        drop(state);
                        let mut callbacks = self.callbacks.borrow_mut();
                        if let Some(appearance_changed) = callbacks.appearance_changed.as_mut() {
                            appearance_changed();
                        }
                    }
                }
                {
                    let mut state = self.state.borrow_mut();

                    if let Some(mut configure) = state.in_progress_configure.take() {
                        let got_unmaximized = state.maximized && !configure.maximized;

                        state.fullscreen = configure.fullscreen;
                        state.maximized = configure.maximized;
                        state.tiling = configure.tiling;
                        if !configure.fullscreen && !configure.maximized {
                            configure.size = if got_unmaximized {
                                Some(state.window_bounds.size)
                            } else {
                                compute_outer_size(state.inset, configure.size, state.tiling)
                            };
                            if let Some(size) = configure.size {
                                state.window_bounds = Bounds {
                                    origin: Point::default(),
                                    size,
                                };
                            }
                        }
                        drop(state);
                        if let Some(size) = configure.size {
                            self.resize(size);
                        }
                    }
                }
                let mut state = self.state.borrow_mut();
                let xdg_surface = state.surface.xdg().unwrap();
                xdg_surface.ack_configure(serial);

                let window_geometry = inset_by_tiling(
                    state.bounds.map_origin(|_| px(0.0)),
                    state.inset.unwrap_or(px(0.0)),
                    state.tiling,
                )
                .map(|v| v.0 as i32)
                .map_size(|v| if v <= 0 { 1 } else { v });

                xdg_surface.set_window_geometry(
                    window_geometry.origin.x,
                    window_geometry.origin.y,
                    window_geometry.size.width,
                    window_geometry.size.height,
                );

                let request_frame_callback = !state.acknowledged_first_configure;
                if request_frame_callback {
                    state.acknowledged_first_configure = true;
                    drop(state);
                    self.frame();
                }
            }
            _ => {}
        }
    }

    pub fn handle_layer_surface(&self, event: zwlr_layer_surface_v1::Event) {
        let mut state = self.state.borrow_mut();
        if state.surface.layer().is_none() {
            log::error!("layer_surface is missing");
            return;
        }
        match event {
            zwlr_layer_surface_v1::Event::Configure {
                serial,
                width,
                height,
            } => {
                let layer_surface = state.surface.layer().unwrap();
                layer_surface.ack_configure(serial);
                layer_surface.set_size(width, height);

                let request_frame_callback = !state.acknowledged_first_configure;
                if request_frame_callback {
                    state.acknowledged_first_configure = true;
                    drop(state);
                    self.frame();
                }
            }
            _ => {}
        }
    }
    pub fn handle_toplevel_decoration_event(&self, event: zxdg_toplevel_decoration_v1::Event) {
        match event {
            zxdg_toplevel_decoration_v1::Event::Configure { mode } => match mode {
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ServerSide) => {
                    self.state.borrow_mut().decorations = WindowDecorations::Server;
                    if let Some(mut appearance_changed) =
                        self.callbacks.borrow_mut().appearance_changed.as_mut()
                    {
                        appearance_changed();
                    }
                }
                WEnum::Value(zxdg_toplevel_decoration_v1::Mode::ClientSide) => {
                    self.state.borrow_mut().decorations = WindowDecorations::Client;
                    // Update background to be transparent
                    if let Some(mut appearance_changed) =
                        self.callbacks.borrow_mut().appearance_changed.as_mut()
                    {
                        appearance_changed();
                    }
                }
                WEnum::Value(_) => {
                    log::warn!("Unknown decoration mode");
                }
                WEnum::Unknown(v) => {
                    log::warn!("Unknown decoration mode: {}", v);
                }
            },
            _ => {}
        }
    }

    pub fn handle_fractional_scale_event(&self, event: wp_fractional_scale_v1::Event) {
        match event {
            wp_fractional_scale_v1::Event::PreferredScale { scale } => {
                self.rescale(scale as f32 / 120.0);
            }
            _ => {}
        }
    }

    pub fn handle_toplevel_event(&self, event: xdg_toplevel::Event) -> bool {
        match event {
            xdg_toplevel::Event::Configure {
                width,
                height,
                states,
            } => {
                let mut size = if width == 0 || height == 0 {
                    None
                } else {
                    Some(size(px(width as f32), px(height as f32)))
                };

                let states = extract_states::<xdg_toplevel::State>(&states);

                let mut tiling = Tiling::default();
                let mut fullscreen = false;
                let mut maximized = false;

                for state in states {
                    match state {
                        xdg_toplevel::State::Maximized => {
                            maximized = true;
                        }
                        xdg_toplevel::State::Fullscreen => {
                            fullscreen = true;
                        }
                        xdg_toplevel::State::TiledTop => {
                            tiling.top = true;
                        }
                        xdg_toplevel::State::TiledLeft => {
                            tiling.left = true;
                        }
                        xdg_toplevel::State::TiledRight => {
                            tiling.right = true;
                        }
                        xdg_toplevel::State::TiledBottom => {
                            tiling.bottom = true;
                        }
                        _ => {
                            // noop
                        }
                    }
                }

                if fullscreen || maximized {
                    tiling = Tiling::tiled();
                }

                let mut state = self.state.borrow_mut();
                state.in_progress_configure = Some(InProgressConfigure {
                    size,
                    fullscreen,
                    maximized,
                    tiling,
                });

                false
            }
            xdg_toplevel::Event::Close => {
                let mut cb = self.callbacks.borrow_mut();
                if let Some(mut should_close) = cb.should_close.take() {
                    let result = (should_close)();
                    cb.should_close = Some(should_close);
                    if result {
                        drop(cb);
                        self.close();
                    }
                    result
                } else {
                    true
                }
            }
            xdg_toplevel::Event::WmCapabilities { capabilities } => {
                let mut window_controls = WindowControls::default();

                let states = extract_states::<xdg_toplevel::WmCapabilities>(&capabilities);

                for state in states {
                    match state {
                        xdg_toplevel::WmCapabilities::Maximize => {
                            window_controls.maximize = true;
                        }
                        xdg_toplevel::WmCapabilities::Minimize => {
                            window_controls.minimize = true;
                        }
                        xdg_toplevel::WmCapabilities::Fullscreen => {
                            window_controls.fullscreen = true;
                        }
                        xdg_toplevel::WmCapabilities::WindowMenu => {
                            window_controls.window_menu = true;
                        }
                        _ => {}
                    }
                }

                let mut state = self.state.borrow_mut();
                state.in_progress_window_controls = Some(window_controls);
                false
            }
            _ => false,
        }
    }

    #[allow(clippy::mutable_key_type)]
    pub fn handle_surface_event(
        &self,
        event: wl_surface::Event,
        outputs: HashMap<ObjectId, Output>,
    ) {
        let mut state = self.state.borrow_mut();

        match event {
            wl_surface::Event::Enter { output } => {
                let id = output.id();

                let Some(output) = outputs.get(&id) else {
                    return;
                };

                state.outputs.insert(id, output.clone());

                let scale = state.primary_output_scale();

                // We use `PreferredBufferScale` instead to set the scale if it's available
                if state.wl_surface.version() < wl_surface::EVT_PREFERRED_BUFFER_SCALE_SINCE {
                    state.wl_surface.set_buffer_scale(scale);
                    drop(state);
                    self.rescale(scale as f32);
                }
            }
            wl_surface::Event::Leave { output } => {
                state.outputs.remove(&output.id());

                let scale = state.primary_output_scale();

                // We use `PreferredBufferScale` instead to set the scale if it's available
                if state.wl_surface.version() < wl_surface::EVT_PREFERRED_BUFFER_SCALE_SINCE {
                    state.wl_surface.set_buffer_scale(scale);
                    drop(state);
                    self.rescale(scale as f32);
                }
            }
            wl_surface::Event::PreferredBufferScale { factor } => {
                // We use `WpFractionalScale` instead to set the scale if it's available
                if state.globals.fractional_scale_manager.is_none() {
                    state.wl_surface.set_buffer_scale(factor);
                    drop(state);
                    self.rescale(factor as f32);
                }
            }
            _ => {}
        }
    }

    pub fn handle_ime(&self, ime: ImeInput) {
        let mut state = self.state.borrow_mut();
        if let Some(mut input_handler) = state.input_handler.take() {
            drop(state);
            match ime {
                ImeInput::InsertText(text) => {
                    input_handler.replace_text_in_range(None, &text);
                }
                ImeInput::SetMarkedText(text) => {
                    input_handler.replace_and_mark_text_in_range(None, &text, None);
                }
                ImeInput::UnmarkText => {
                    input_handler.unmark_text();
                }
                ImeInput::DeleteText => {
                    if let Some(marked) = input_handler.marked_text_range() {
                        input_handler.replace_text_in_range(Some(marked), "");
                    }
                }
            }
            self.state.borrow_mut().input_handler = Some(input_handler);
        }
    }

    pub fn get_ime_area(&self) -> Option<Bounds<Pixels>> {
        let mut state = self.state.borrow_mut();
        let mut bounds: Option<Bounds<Pixels>> = None;
        if let Some(mut input_handler) = state.input_handler.take() {
            drop(state);
            if let Some(selection) = input_handler.selected_text_range(true) {
                bounds = input_handler.bounds_for_range(if selection.reversed {
                    selection.range.start..selection.range.start
                } else {
                    selection.range.end..selection.range.end
                });
            }
            self.state.borrow_mut().input_handler = Some(input_handler);
        }
        bounds
    }

    pub fn set_size_and_scale(&self, size: Option<Size<Pixels>>, scale: Option<f32>) {
        let (size, scale) = {
            let mut state = self.state.borrow_mut();
            if size.map_or(true, |size| size == state.bounds.size)
                && scale.map_or(true, |scale| scale == state.scale)
            {
                return;
            }
            if let Some(size) = size {
                state.bounds.size = size;
            }
            if let Some(scale) = scale {
                state.scale = scale;
            }
            let device_bounds = state.bounds.to_device_pixels(state.scale);
            state.renderer.update_drawable_size(device_bounds.size);
            (state.bounds.size, state.scale)
        };

        if let Some(ref mut fun) = self.callbacks.borrow_mut().resize {
            fun(size, scale);
        }

        {
            let state = self.state.borrow();
            if let Some(viewport) = &state.viewport {
                viewport.set_destination(size.width.0 as i32, size.height.0 as i32);
            }
        }
    }

    pub fn resize(&self, size: Size<Pixels>) {
        self.set_size_and_scale(Some(size), None);
    }

    pub fn rescale(&self, scale: f32) {
        self.set_size_and_scale(None, Some(scale));
    }

    pub fn close(&self) {
        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(fun) = callbacks.close.take() {
            fun()
        }
    }

    pub fn handle_input(&self, input: PlatformInput) {
        if let Some(ref mut fun) = self.callbacks.borrow_mut().input {
            if !fun(input.clone()).propagate {
                return;
            }
        }
        if let PlatformInput::KeyDown(event) = input {
            if let Some(key_char) = &event.keystroke.key_char {
                let mut state = self.state.borrow_mut();
                if let Some(mut input_handler) = state.input_handler.take() {
                    drop(state);
                    input_handler.replace_text_in_range(None, key_char);
                    self.state.borrow_mut().input_handler = Some(input_handler);
                }
            }
        }
    }

    pub fn set_focused(&self, focus: bool) {
        self.state.borrow_mut().active = focus;
        if let Some(ref mut fun) = self.callbacks.borrow_mut().active_status_change {
            fun(focus);
        }
    }

    pub fn set_hovered(&self, focus: bool) {
        if let Some(ref mut fun) = self.callbacks.borrow_mut().hover_status_change {
            fun(focus);
        }
    }

    pub fn set_appearance(&mut self, appearance: WindowAppearance) {
        self.state.borrow_mut().appearance = appearance;

        let mut callbacks = self.callbacks.borrow_mut();
        if let Some(ref mut fun) = callbacks.appearance_changed {
            (fun)()
        }
    }

    pub fn primary_output_scale(&self) -> i32 {
        self.state.borrow_mut().primary_output_scale()
    }
}

fn extract_states<'a, S: TryFrom<u32> + 'a>(states: &'a [u8]) -> impl Iterator<Item = S> + 'a
where
    <S as TryFrom<u32>>::Error: 'a,
{
    states
        .chunks_exact(4)
        .flat_map(TryInto::<[u8; 4]>::try_into)
        .map(u32::from_ne_bytes)
        .flat_map(S::try_from)
}

impl rwh::HasWindowHandle for WaylandWindow {
    fn window_handle(&self) -> Result<rwh::WindowHandle<'_>, rwh::HandleError> {
        unimplemented!()
    }
}
impl rwh::HasDisplayHandle for WaylandWindow {
    fn display_handle(&self) -> Result<rwh::DisplayHandle<'_>, rwh::HandleError> {
        unimplemented!()
    }
}

impl PlatformWindow for WaylandWindow {
    fn bounds(&self) -> Bounds<Pixels> {
        self.borrow().bounds
    }

    fn is_maximized(&self) -> bool {
        self.borrow().maximized
    }

    fn window_bounds(&self) -> WindowBounds {
        let state = self.borrow();
        if state.fullscreen {
            WindowBounds::Fullscreen(state.window_bounds)
        } else if state.maximized {
            WindowBounds::Maximized(state.window_bounds)
        } else {
            drop(state);
            WindowBounds::Windowed(self.bounds())
        }
    }

    fn inner_window_bounds(&self) -> WindowBounds {
        let state = self.borrow();
        if state.fullscreen {
            WindowBounds::Fullscreen(state.window_bounds)
        } else if state.maximized {
            WindowBounds::Maximized(state.window_bounds)
        } else {
            let inset = state.inset.unwrap_or(px(0.));
            drop(state);
            WindowBounds::Windowed(self.bounds().inset(inset))
        }
    }

    fn content_size(&self) -> Size<Pixels> {
        self.borrow().bounds.size
    }

    fn scale_factor(&self) -> f32 {
        self.borrow().scale
    }

    fn appearance(&self) -> WindowAppearance {
        self.borrow().appearance
    }

    fn display(&self) -> Option<Rc<dyn PlatformDisplay>> {
        let state = self.borrow();
        state.display.as_ref().map(|(id, display)| {
            Rc::new(WaylandDisplay {
                id: id.clone(),
                name: display.name.clone(),
                bounds: display.bounds.to_pixels(state.scale),
            }) as Rc<dyn PlatformDisplay>
        })
    }

    fn mouse_position(&self) -> Point<Pixels> {
        self.borrow()
            .client
            .get_client()
            .borrow()
            .mouse_location
            .unwrap_or_default()
    }

    fn modifiers(&self) -> Modifiers {
        self.borrow().client.get_client().borrow().modifiers
    }

    fn set_input_handler(&mut self, input_handler: PlatformInputHandler) {
        self.borrow_mut().input_handler = Some(input_handler);
    }

    fn take_input_handler(&mut self) -> Option<PlatformInputHandler> {
        self.borrow_mut().input_handler.take()
    }

    fn prompt(
        &self,
        _level: PromptLevel,
        _msg: &str,
        _detail: Option<&str>,
        _answers: &[&str],
    ) -> Option<Receiver<usize>> {
        None
    }

    fn activate(&self) {
        // Try to request an activation token. Even though the activation is likely going to be rejected,
        // KWin and Mutter can use the app_id to visually indicate we're requesting attention.
        let state = self.borrow();
        if let (Some(activation), Some(app_id)) = (&state.globals.activation, state.app_id.clone())
        {
            state.client.set_pending_activation(state.wl_surface.id());
            let token = activation.get_activation_token(&state.globals.qh, ());
            // The serial isn't exactly important here, since the activation is probably going to be rejected anyway.
            let serial = state.client.get_serial(SerialKind::MousePress);
            token.set_app_id(app_id);
            token.set_serial(serial, &state.globals.seat);
            token.set_surface(&state.wl_surface);
            token.commit();
        }
    }

    fn is_active(&self) -> bool {
        self.borrow().active
    }

    fn is_hovered(&self) -> bool {
        self.borrow().hovered
    }

    fn set_title(&mut self, title: &str) {
        match self.borrow().surface.toplevel() {
            Some(toplevel) => toplevel.set_title(title.to_string()),
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn set_app_id(&mut self, app_id: &str) {
        let mut state = self.borrow_mut();
        match state.surface.toplevel() {
            Some(toplevel) => {
                toplevel.set_app_id(app_id.to_owned());
                state.app_id = Some(app_id.to_owned());
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn set_background_appearance(&self, background_appearance: WindowBackgroundAppearance) {
        let mut state = self.borrow_mut();
        state.background_appearance = background_appearance;
        update_window(state);
    }

    fn minimize(&self) {
        match self.borrow().surface.toplevel() {
            Some(toplevel) => toplevel.set_minimized(),
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn zoom(&self) {
        let state = self.borrow();
        match state.surface.toplevel() {
            Some(toplevel) => {
                if !state.maximized {
                    toplevel.set_maximized();
                } else {
                    toplevel.unset_maximized();
                }
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn toggle_fullscreen(&self) {
        let mut state = self.borrow_mut();
        match state.surface.toplevel() {
            Some(toplevel) => {
                if !state.fullscreen {
                    toplevel.set_fullscreen(None);
                } else {
                    toplevel.unset_fullscreen();
                }
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn is_fullscreen(&self) -> bool {
        self.borrow().fullscreen
    }

    fn on_request_frame(&self, callback: Box<dyn FnMut(RequestFrameOptions)>) {
        self.0.callbacks.borrow_mut().request_frame = Some(callback);
    }

    fn on_input(&self, callback: Box<dyn FnMut(PlatformInput) -> crate::DispatchEventResult>) {
        self.0.callbacks.borrow_mut().input = Some(callback);
    }

    fn on_active_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().active_status_change = Some(callback);
    }

    fn on_hover_status_change(&self, callback: Box<dyn FnMut(bool)>) {
        self.0.callbacks.borrow_mut().hover_status_change = Some(callback);
    }

    fn on_resize(&self, callback: Box<dyn FnMut(Size<Pixels>, f32)>) {
        self.0.callbacks.borrow_mut().resize = Some(callback);
    }

    fn on_moved(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().moved = Some(callback);
    }

    fn on_should_close(&self, callback: Box<dyn FnMut() -> bool>) {
        self.0.callbacks.borrow_mut().should_close = Some(callback);
    }

    fn on_close(&self, callback: Box<dyn FnOnce()>) {
        self.0.callbacks.borrow_mut().close = Some(callback);
    }

    fn on_appearance_changed(&self, callback: Box<dyn FnMut()>) {
        self.0.callbacks.borrow_mut().appearance_changed = Some(callback);
    }

    fn draw(&self, scene: &Scene) {
        let mut state = self.borrow_mut();
        state.renderer.draw(scene);
    }

    fn completed_frame(&self) {
        let state = self.borrow();
        state.wl_surface.commit();
    }

    fn sprite_atlas(&self) -> Arc<dyn PlatformAtlas> {
        let state = self.borrow();
        state.renderer.sprite_atlas().clone()
    }

    fn show_window_menu(&self, position: Point<Pixels>) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);
        match state.surface.toplevel() {
            Some(toplevel) => {
                toplevel.show_window_menu(
                    &state.globals.seat,
                    serial,
                    position.x.0 as i32,
                    position.y.0 as i32,
                );
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn start_window_move(&self) {
        let state = self.borrow();
        let serial = state.client.get_serial(SerialKind::MousePress);

        match state.surface.toplevel() {
            Some(toplevel) => {
                toplevel._move(&state.globals.seat, serial);
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn start_window_resize(&self, edge: crate::ResizeEdge) {
        let state = self.borrow();
        match state.surface.toplevel() {
            Some(toplevel) => {
                toplevel.resize(
                    &state.globals.seat,
                    state.client.get_serial(SerialKind::MousePress),
                    edge.to_xdg(),
                );
            }
            None => log::error!("not a xdg wl_surface"),
        }
    }

    fn window_decorations(&self) -> Decorations {
        let state = self.borrow();
        match state.decorations {
            WindowDecorations::Server => Decorations::Server,
            WindowDecorations::Client => Decorations::Client {
                tiling: state.tiling,
            },
        }
    }

    fn request_decorations(&self, decorations: WindowDecorations) {
        let mut state = self.borrow_mut();
        state.decorations = decorations;
        match state.surface.decoration() {
            Some(decoration) => {
                decoration.set_mode(decorations.to_xdg());
                update_window(state);
            }
            None => log::error!("not a xdg surface"),
        }
    }

    fn window_controls(&self) -> WindowControls {
        self.borrow().window_controls
    }

    fn set_client_inset(&self, inset: Pixels) {
        let mut state = self.borrow_mut();
        if Some(inset) != state.inset {
            state.inset = Some(inset);
            update_window(state);
        }
    }

    fn update_ime_position(&self, bounds: Bounds<ScaledPixels>) {
        let state = self.borrow();
        state.client.update_ime_position(bounds);
    }

    fn gpu_specs(&self) -> Option<GpuSpecs> {
        self.borrow().renderer.gpu_specs().into()
    }
}

fn update_window(mut state: RefMut<WaylandWindowState>) {
    let opaque = !state.is_transparent();

    state.renderer.update_transparency(!opaque);
    let mut opaque_area = state.window_bounds.map(|v| v.0 as i32);
    if let Some(inset) = state.inset {
        opaque_area.inset(inset.0 as i32);
    }

    let region = state
        .globals
        .compositor
        .create_region(&state.globals.qh, ());
    region.add(
        opaque_area.origin.x,
        opaque_area.origin.y,
        opaque_area.size.width,
        opaque_area.size.height,
    );

    // Note that rounded corners make this rectangle API hard to work with.
    // As this is common when using CSD, let's just disable this API.
    if state.background_appearance == WindowBackgroundAppearance::Opaque
        && state.decorations == WindowDecorations::Server
    {
        // Promise the compositor that this region of the window surface
        // contains no transparent pixels. This allows the compositor to skip
        // updating whatever is behind the surface for better performance.
        state.wl_surface.set_opaque_region(Some(&region));
    } else {
        state.wl_surface.set_opaque_region(None);
    }

    if let Some(ref blur_manager) = state.globals.blur_manager {
        if state.background_appearance == WindowBackgroundAppearance::Blurred {
            if state.blur.is_none() {
                let blur = blur_manager.create(&state.wl_surface, &state.globals.qh, ());
                state.blur = Some(blur);
            }
            state.blur.as_ref().unwrap().commit();
        } else {
            // It probably doesn't hurt to clear the blur for opaque windows
            blur_manager.unset(&state.wl_surface);
            if let Some(b) = state.blur.take() {
                b.release()
            }
        }
    }

    region.destroy();
}

impl WindowDecorations {
    fn to_xdg(&self) -> zxdg_toplevel_decoration_v1::Mode {
        match self {
            WindowDecorations::Client => zxdg_toplevel_decoration_v1::Mode::ClientSide,
            WindowDecorations::Server => zxdg_toplevel_decoration_v1::Mode::ServerSide,
        }
    }
}

impl ResizeEdge {
    fn to_xdg(&self) -> xdg_toplevel::ResizeEdge {
        match self {
            ResizeEdge::Top => xdg_toplevel::ResizeEdge::Top,
            ResizeEdge::TopRight => xdg_toplevel::ResizeEdge::TopRight,
            ResizeEdge::Right => xdg_toplevel::ResizeEdge::Right,
            ResizeEdge::BottomRight => xdg_toplevel::ResizeEdge::BottomRight,
            ResizeEdge::Bottom => xdg_toplevel::ResizeEdge::Bottom,
            ResizeEdge::BottomLeft => xdg_toplevel::ResizeEdge::BottomLeft,
            ResizeEdge::Left => xdg_toplevel::ResizeEdge::Left,
            ResizeEdge::TopLeft => xdg_toplevel::ResizeEdge::TopLeft,
        }
    }
}

/// The configuration event is in terms of the window geometry, which we are constantly
/// updating to account for the client decorations. But that's not the area we want to render
/// to, due to our intrusize CSD. So, here we calculate the 'actual' size, by adding back in the insets
fn compute_outer_size(
    inset: Option<Pixels>,
    new_size: Option<Size<Pixels>>,
    tiling: Tiling,
) -> Option<Size<Pixels>> {
    let Some(inset) = inset else { return new_size };

    new_size.map(|mut new_size| {
        if !tiling.top {
            new_size.height += inset;
        }
        if !tiling.bottom {
            new_size.height += inset;
        }
        if !tiling.left {
            new_size.width += inset;
        }
        if !tiling.right {
            new_size.width += inset;
        }

        new_size
    })
}

fn inset_by_tiling(mut bounds: Bounds<Pixels>, inset: Pixels, tiling: Tiling) -> Bounds<Pixels> {
    if !tiling.top {
        bounds.origin.y += inset;
        bounds.size.height -= inset;
    }
    if !tiling.bottom {
        bounds.size.height -= inset;
    }
    if !tiling.left {
        bounds.origin.x += inset;
        bounds.size.width -= inset;
    }
    if !tiling.right {
        bounds.size.width -= inset;
    }

    bounds
}
