// Copyright 2019-2024 Tauri Programme within The Commons Conservancy
// SPDX-License-Identifier: Apache-2.0
// SPDX-License-Identifier: MIT

use crate::{
  image::Image,
  ipc::{
    channel::ChannelDataIpcQueue, CommandArg, CommandItem, Invoke, InvokeError, InvokeHandler,
  },
  manager::{webview::UriSchemeProtocol, AppManager, Asset},
  plugin::{Plugin, PluginStore},
  resources::ResourceTable,
  runtime::{
    window::{WebviewEvent as RuntimeWebviewEvent, WindowEvent as RuntimeWindowEvent},
    ExitRequestedEventAction, RunEvent as RuntimeRunEvent,
  },
  sealed::{ManagerBase, RuntimeOrDispatch},
  utils::config::Config,
  utils::Env,
  webview::PageLoadPayload,
  Context, DeviceEventFilter, Emitter, EventLoopMessage, Listener, Manager, Monitor, Result,
  Runtime, Scopes, StateManager, Theme, Webview, WebviewWindowBuilder, Window,
};

#[cfg(desktop)]
use crate::menu::{Menu, MenuEvent};
#[cfg(all(desktop, feature = "tray-icon"))]
use crate::tray::{TrayIcon, TrayIconBuilder, TrayIconEvent, TrayIconId};
use serialize_to_javascript::{default_template, DefaultTemplate, Template};
use tauri_macros::default_runtime;
#[cfg(desktop)]
use tauri_runtime::EventLoopProxy;
use tauri_runtime::{
  dpi::{PhysicalPosition, PhysicalSize},
  window::{DragDropEvent, ElementState, ModifiersState, MouseButton},
  RuntimeInitArgs,
};
use tauri_utils::{assets::AssetsIter, PackageInfo};

use serde::Serialize;
use std::{
  borrow::Cow,
  collections::HashMap,
  fmt,
  sync::{mpsc::Sender, Arc, MutexGuard},
};

use crate::{event::EventId, runtime::RuntimeHandle, Event, EventTarget};

#[cfg(target_os = "macos")]
use crate::ActivationPolicy;

pub(crate) mod plugin;

#[cfg(desktop)]
pub(crate) type GlobalMenuEventListener<T> = Box<dyn Fn(&T, crate::menu::MenuEvent) + Send + Sync>;
#[cfg(all(desktop, feature = "tray-icon"))]
pub(crate) type GlobalTrayIconEventListener<T> =
  Box<dyn Fn(&T, crate::tray::TrayIconEvent) + Send + Sync>;
pub(crate) type GlobalWindowEventListener<R> = Box<dyn Fn(&Window<R>, &WindowEvent) + Send + Sync>;
pub(crate) type GlobalWebviewEventListener<R> =
  Box<dyn Fn(&Webview<R>, &WebviewEvent) + Send + Sync>;
/// A closure that is run when the Tauri application is setting up.
pub type SetupHook<R> =
  Box<dyn FnOnce(&mut App<R>) -> std::result::Result<(), Box<dyn std::error::Error>> + Send>;
/// A closure that is run every time a page starts or finishes loading.
pub type OnPageLoad<R> = dyn Fn(&Webview<R>, &PageLoadPayload<'_>) + Send + Sync + 'static;

/// The exit code on [`RunEvent::ExitRequested`] when [`AppHandle#method.restart`] is called.
pub const RESTART_EXIT_CODE: i32 = i32::MAX;

/// Api exposed on the `ExitRequested` event.
#[derive(Debug)]
pub struct ExitRequestApi(Sender<ExitRequestedEventAction>);

impl ExitRequestApi {
  /// Prevents the app from exiting.
  ///
  /// **Note:** This is ignored when using [`AppHandle#method.restart`].
  pub fn prevent_exit(&self) {
    self.0.send(ExitRequestedEventAction::Prevent).unwrap();
  }
}

/// Api exposed on the `CloseRequested` event.
#[derive(Debug, Clone)]
pub struct CloseRequestApi(Sender<bool>);

impl CloseRequestApi {
  /// Prevents the window from being closed.
  pub fn prevent_close(&self) {
    self.0.send(true).unwrap();
  }
}

/// An event from a window.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WindowEvent {
  /// The size of the window has changed. Contains the client area's new dimensions.
  Resized(PhysicalSize<u32>),
  /// The position of the window has changed. Contains the window's new position.
  Moved(PhysicalPosition<i32>),
  /// An mouse button press has been received.
  MouseInput {
    state: ElementState,
    button: MouseButton,
    #[deprecated = "Deprecated in favor of WindowEvent::ModifiersChanged"]
    modifiers: ModifiersState,
  },
  /// The window has been requested to close.
  #[non_exhaustive]
  CloseRequested {
    /// An API modify the behavior of the close requested event.
    api: CloseRequestApi,
  },
  /// The window has been destroyed.
  Destroyed,
  /// The window gained or lost focus.
  ///
  /// The parameter is true if the window has gained focus, and false if it has lost focus.
  Focused(bool),
  /// The window's scale factor has changed.
  ///
  /// The following user actions can cause DPI changes:
  ///
  /// - Changing the display's resolution.
  /// - Changing the display's scale factor (e.g. in Control Panel on Windows).
  /// - Moving the window to a display with a different scale factor.
  #[non_exhaustive]
  ScaleFactorChanged {
    /// The new scale factor.
    scale_factor: f64,
    /// The window inner size.
    new_inner_size: PhysicalSize<u32>,
  },
  /// An event associated with the drag and drop action.
  DragDrop(DragDropEvent),
  /// The system window theme has changed. Only delivered if the window [`theme`](`crate::window::WindowBuilder#method.theme`) is `None`.
  ///
  /// Applications might wish to react to this to change the theme of the content of the window when the system changes the window theme.
  ///
  /// ## Platform-specific
  ///
  /// - **Linux**: Not supported.
  ThemeChanged(Theme),
}

impl From<RuntimeWindowEvent> for WindowEvent {
  fn from(event: RuntimeWindowEvent) -> Self {
    match event {
      RuntimeWindowEvent::Resized(size) => Self::Resized(size),
      RuntimeWindowEvent::Moved(position) => Self::Moved(position),
      RuntimeWindowEvent::CloseRequested { signal_tx } => Self::CloseRequested {
        api: CloseRequestApi(signal_tx),
      },
      RuntimeWindowEvent::Destroyed => Self::Destroyed,
      RuntimeWindowEvent::Focused(flag) => Self::Focused(flag),
      RuntimeWindowEvent::ScaleFactorChanged {
        scale_factor,
        new_inner_size,
      } => Self::ScaleFactorChanged {
        scale_factor,
        new_inner_size,
      },
      RuntimeWindowEvent::DragDrop(event) => Self::DragDrop(event),
      RuntimeWindowEvent::ThemeChanged(theme) => Self::ThemeChanged(theme),
      RuntimeWindowEvent::MouseInput {
        state,
        button,
        modifiers,
      } => Self::MouseInput {
        state,
        button,
        modifiers,
      },
    }
  }
}

/// An event from a window.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub enum WebviewEvent {
  /// An event associated with the drag and drop action.
  DragDrop(DragDropEvent),
}

impl From<RuntimeWebviewEvent> for WebviewEvent {
  fn from(event: RuntimeWebviewEvent) -> Self {
    match event {
      RuntimeWebviewEvent::DragDrop(e) => Self::DragDrop(e),
    }
  }
}

/// An application event, triggered from the event loop.
///
/// See [`App::run`](crate::App#method.run) for usage examples.
#[derive(Debug)]
#[non_exhaustive]
pub enum RunEvent {
  /// Event loop is exiting.
  Exit,
  /// The app is about to exit
  #[non_exhaustive]
  ExitRequested {
    /// Exit code.
    /// [`Option::None`] when the exit is requested by user interaction,
    /// [`Option::Some`] when requested programmatically via [`AppHandle#method.exit`] and [`AppHandle#method.restart`].
    code: Option<i32>,
    /// Event API
    api: ExitRequestApi,
  },
  /// An event associated with a window.
  #[non_exhaustive]
  WindowEvent {
    /// The window label.
    label: String,
    /// The detailed event.
    event: WindowEvent,
  },
  /// An event associated with a webview.
  #[non_exhaustive]
  WebviewEvent {
    /// The window label.
    label: String,
    /// The detailed event.
    event: WebviewEvent,
  },
  /// Application ready.
  Ready,
  /// Sent if the event loop is being resumed.
  Resumed,
  /// Emitted when all of the event loop's input events have been processed and redraw processing is about to begin.
  ///
  /// This event is useful as a place to put your code that should be run after all state-changing events have been handled and you want to do stuff (updating state, performing calculations, etc) that happens as the “main body” of your event loop.
  MainEventsCleared,
  /// Emitted when the user wants to open the specified resource with the app.
  #[cfg(any(target_os = "macos", target_os = "ios"))]
  #[cfg_attr(docsrs, doc(cfg(any(target_os = "macos", feature = "ios"))))]
  Opened {
    /// The URL of the resources that is being open.
    urls: Vec<url::Url>,
  },
  /// An event from a menu item, could be on the window menu bar, application menu bar (on macOS) or tray icon menu.
  #[cfg(desktop)]
  #[cfg_attr(docsrs, doc(cfg(desktop)))]
  MenuEvent(crate::menu::MenuEvent),
  /// An event from a tray icon.
  #[cfg(all(desktop, feature = "tray-icon"))]
  #[cfg_attr(docsrs, doc(cfg(all(desktop, feature = "tray-icon"))))]
  TrayIconEvent(crate::tray::TrayIconEvent),
  /// Emitted when the NSApplicationDelegate's applicationShouldHandleReopen gets called
  #[non_exhaustive]
  #[cfg(target_os = "macos")]
  #[cfg_attr(docsrs, doc(cfg(target_os = "macos")))]
  Reopen {
    /// Indicates whether the NSApplication object found any visible windows in your application.
    has_visible_windows: bool,
  },
}

impl From<EventLoopMessage> for RunEvent {
  fn from(event: EventLoopMessage) -> Self {
    match event {
      #[cfg(desktop)]
      EventLoopMessage::MenuEvent(e) => Self::MenuEvent(e),
      #[cfg(all(desktop, feature = "tray-icon"))]
      EventLoopMessage::TrayIconEvent(e) => Self::TrayIconEvent(e),
    }
  }
}

/// The asset resolver is a helper to access the [`tauri_utils::assets::Assets`] interface.
#[derive(Debug, Clone)]
pub struct AssetResolver<R: Runtime> {
  manager: Arc<AppManager<R>>,
}

impl<R: Runtime> AssetResolver<R> {
  /// Gets the app asset associated with the given path.
  ///
  /// Resolves to the embedded asset that is part of the app
  /// in dev when [`devUrl`](https://v2.tauri.app/reference/config/#devurl) points to a folder in your filesystem
  /// or in production when [`frontendDist`](https://v2.tauri.app/reference/config/#frontenddist)
  /// points to your frontend assets.
  ///
  /// Fallbacks to reading the asset from the [distDir] folder so the behavior is consistent in development.
  /// Note that the dist directory must exist so you might need to build your frontend assets first.
  pub fn get(&self, path: String) -> Option<Asset> {
    #[cfg(dev)]
    {
      // on dev if the devPath is a path to a directory we have the embedded assets
      // so we can use get_asset() directly
      // we only fallback to reading from distDir directly if we're using an external URL (which is likely)
      if let (Some(_), Some(crate::utils::config::FrontendDist::Directory(dist_path))) = (
        &self.manager.config().build.dev_url,
        &self.manager.config().build.frontend_dist,
      ) {
        let asset_path = std::path::PathBuf::from(&path)
          .components()
          .filter(|c| !matches!(c, std::path::Component::RootDir))
          .collect::<std::path::PathBuf>();

        let asset_path = self
          .manager
          .config_parent()
          .map(|p| p.join(dist_path).join(&asset_path))
          .unwrap_or_else(|| dist_path.join(&asset_path));
        return std::fs::read(asset_path).ok().map(|bytes| {
          let mime_type = crate::utils::mime_type::MimeType::parse(&bytes, &path);
          Asset {
            bytes,
            mime_type,
            csp_header: None,
          }
        });
      }
    }

    self.manager.get_asset(path).ok()
  }

  /// Iterate on all assets.
  pub fn iter(&self) -> Box<AssetsIter<'_>> {
    self.manager.assets.iter()
  }
}

/// A handle to the currently running application.
///
/// This type implements [`Manager`] which allows for manipulation of global application items.
#[default_runtime(crate::Wry, wry)]
#[derive(Debug)]
pub struct AppHandle<R: Runtime> {
  pub(crate) runtime_handle: R::Handle,
  pub(crate) manager: Arc<AppManager<R>>,
}

/// APIs specific to the wry runtime.
#[cfg(feature = "wry")]
impl AppHandle<crate::Wry> {
  /// Create a new tao window using a callback. The event loop must be running at this point.
  pub fn create_tao_window<
    F: FnOnce() -> (String, tauri_runtime_wry::TaoWindowBuilder) + Send + 'static,
  >(
    &self,
    f: F,
  ) -> crate::Result<std::sync::Weak<tauri_runtime_wry::Window>> {
    self.runtime_handle.create_tao_window(f).map_err(Into::into)
  }

  /// Sends a window message to the event loop.
  pub fn send_tao_window_event(
    &self,
    window_id: tauri_runtime_wry::TaoWindowId,
    message: tauri_runtime_wry::WindowMessage,
  ) -> crate::Result<()> {
    self
      .runtime_handle
      .send_event(tauri_runtime_wry::Message::Window(
        self.runtime_handle.window_id(window_id),
        message,
      ))
      .map_err(Into::into)
  }
}

impl<R: Runtime> Clone for AppHandle<R> {
  fn clone(&self) -> Self {
    Self {
      runtime_handle: self.runtime_handle.clone(),
      manager: self.manager.clone(),
    }
  }
}

impl<'de, R: Runtime> CommandArg<'de, R> for AppHandle<R> {
  /// Grabs the [`Window`] from the [`CommandItem`] and returns the associated [`AppHandle`]. This will never fail.
  fn from_command(command: CommandItem<'de, R>) -> std::result::Result<Self, InvokeError> {
    Ok(command.message.webview().app_handle)
  }
}

impl<R: Runtime> AppHandle<R> {
  /// Runs the given closure on the main thread.
  pub fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> crate::Result<()> {
    self
      .runtime_handle
      .run_on_main_thread(f)
      .map_err(Into::into)
  }

  /// Adds a Tauri application plugin.
  /// This function can be used to register a plugin that is loaded dynamically e.g. after login.
  /// For plugins that are created when the app is started, prefer [`Builder::plugin`].
  ///
  /// See [`Builder::plugin`] for more information.
  ///
  /// # Examples
  ///
  /// ```
  /// use tauri::{plugin::{Builder as PluginBuilder, TauriPlugin}, Runtime};
  ///
  /// fn init_plugin<R: Runtime>() -> TauriPlugin<R> {
  ///   PluginBuilder::new("dummy").build()
  /// }
  ///
  /// tauri::Builder::default()
  ///   .setup(move |app| {
  ///     let handle = app.handle().clone();
  ///     std::thread::spawn(move || {
  ///       handle.plugin(init_plugin());
  ///     });
  ///
  ///     Ok(())
  ///   });
  /// ```
  #[cfg_attr(feature = "tracing", tracing::instrument(name = "app::plugin::register", skip(plugin), fields(name = plugin.name())))]
  pub fn plugin<P: Plugin<R> + 'static>(&self, plugin: P) -> crate::Result<()> {
    let mut plugin = Box::new(plugin) as Box<dyn Plugin<R>>;

    let mut store = self.manager().plugins.lock().unwrap();
    store.initialize(&mut plugin, self, &self.config().plugins)?;
    store.register(plugin);

    Ok(())
  }

  /// Removes the plugin with the given name.
  ///
  /// # Examples
  ///
  /// ```
  /// use tauri::{plugin::{Builder as PluginBuilder, TauriPlugin, Plugin}, Runtime};
  ///
  /// fn init_plugin<R: Runtime>() -> TauriPlugin<R> {
  ///   PluginBuilder::new("dummy").build()
  /// }
  ///
  /// let plugin = init_plugin();
  /// // `.name()` requires the `PLugin` trait import
  /// let plugin_name = plugin.name();
  /// tauri::Builder::default()
  ///   .plugin(plugin)
  ///   .setup(move |app| {
  ///     let handle = app.handle().clone();
  ///     std::thread::spawn(move || {
  ///       handle.remove_plugin(plugin_name);
  ///     });
  ///
  ///     Ok(())
  ///   });
  /// ```
  pub fn remove_plugin(&self, plugin: &'static str) -> bool {
    self.manager().plugins.lock().unwrap().unregister(plugin)
  }

  /// Exits the app by triggering [`RunEvent::ExitRequested`] and [`RunEvent::Exit`].
  pub fn exit(&self, exit_code: i32) {
    if let Err(e) = self.runtime_handle.request_exit(exit_code) {
      log::error!("failed to exit: {}", e);
      self.cleanup_before_exit();
      std::process::exit(exit_code);
    }
  }

  /// Restarts the app by triggering [`RunEvent::ExitRequested`] with code [`RESTART_EXIT_CODE`] and [`RunEvent::Exit`]..
  pub fn restart(&self) -> ! {
    if self.runtime_handle.request_exit(RESTART_EXIT_CODE).is_err() {
      self.cleanup_before_exit();
    }
    crate::process::restart(&self.env());
  }

  /// Sets the activation policy for the application. It is set to `NSApplicationActivationPolicyRegular` by default.
  ///
  /// # Examples
  /// ```,no_run
  /// tauri::Builder::default()
  ///   .setup(move |app| {
  ///     #[cfg(target_os = "macos")]
  ///     app.handle().set_activation_policy(tauri::ActivationPolicy::Accessory);
  ///     Ok(())
  ///   });
  /// ```
  #[cfg(target_os = "macos")]
  #[cfg_attr(docsrs, doc(cfg(target_os = "macos")))]
  pub fn set_activation_policy(&self, activation_policy: ActivationPolicy) -> crate::Result<()> {
    self
      .runtime_handle
      .set_activation_policy(activation_policy)
      .map_err(Into::into)
  }
}

impl<R: Runtime> Manager<R> for AppHandle<R> {
  fn resources_table(&self) -> MutexGuard<'_, ResourceTable> {
    self.manager.resources_table()
  }
}

impl<R: Runtime> ManagerBase<R> for AppHandle<R> {
  fn manager(&self) -> &AppManager<R> {
    &self.manager
  }

  fn manager_owned(&self) -> Arc<AppManager<R>> {
    self.manager.clone()
  }

  fn runtime(&self) -> RuntimeOrDispatch<'_, R> {
    RuntimeOrDispatch::RuntimeHandle(self.runtime_handle.clone())
  }

  fn managed_app_handle(&self) -> &AppHandle<R> {
    self
  }
}

/// The instance of the currently running application.
///
/// This type implements [`Manager`] which allows for manipulation of global application items.
#[default_runtime(crate::Wry, wry)]
pub struct App<R: Runtime> {
  runtime: Option<R>,
  setup: Option<SetupHook<R>>,
  manager: Arc<AppManager<R>>,
  handle: AppHandle<R>,
  ran_setup: bool,
}

impl<R: Runtime> fmt::Debug for App<R> {
  fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
    f.debug_struct("App")
      .field("runtime", &self.runtime)
      .field("manager", &self.manager)
      .field("handle", &self.handle)
      .finish()
  }
}

impl<R: Runtime> Manager<R> for App<R> {
  fn resources_table(&self) -> MutexGuard<'_, ResourceTable> {
    self.manager.resources_table()
  }
}

impl<R: Runtime> ManagerBase<R> for App<R> {
  fn manager(&self) -> &AppManager<R> {
    &self.manager
  }

  fn manager_owned(&self) -> Arc<AppManager<R>> {
    self.manager.clone()
  }

  fn runtime(&self) -> RuntimeOrDispatch<'_, R> {
    if let Some(runtime) = self.runtime.as_ref() {
      RuntimeOrDispatch::Runtime(runtime)
    } else {
      self.handle.runtime()
    }
  }

  fn managed_app_handle(&self) -> &AppHandle<R> {
    self.handle()
  }
}

/// APIs specific to the wry runtime.
#[cfg(feature = "wry")]
impl App<crate::Wry> {
  /// Adds a [`tauri_runtime_wry::Plugin`] using its [`tauri_runtime_wry::PluginBuilder`].
  ///
  /// # Stability
  ///
  /// This API is unstable.
  pub fn wry_plugin<P: tauri_runtime_wry::PluginBuilder<EventLoopMessage> + Send + 'static>(
    &mut self,
    plugin: P,
  ) where
    <P as tauri_runtime_wry::PluginBuilder<EventLoopMessage>>::Plugin: Send,
  {
    self.handle.runtime_handle.plugin(plugin);
  }
}

macro_rules! shared_app_impl {
  ($app: ty) => {
    impl<R: Runtime> $app {
      /// Registers a global menu event listener.
      #[cfg(desktop)]
      pub fn on_menu_event<F: Fn(&AppHandle<R>, MenuEvent) + Send + Sync + 'static>(
        &self,
        handler: F,
      ) {
        self.manager.menu.on_menu_event(handler)
      }

      /// Registers a global tray icon menu event listener.
      #[cfg(all(desktop, feature = "tray-icon"))]
      #[cfg_attr(docsrs, doc(cfg(all(desktop, feature = "tray-icon"))))]
      pub fn on_tray_icon_event<F: Fn(&AppHandle<R>, TrayIconEvent) + Send + Sync + 'static>(
        &self,
        handler: F,
      ) {
        self.manager.tray.on_tray_icon_event(handler)
      }

      /// Gets a tray icon using the provided id.
      #[cfg(all(desktop, feature = "tray-icon"))]
      #[cfg_attr(docsrs, doc(cfg(all(desktop, feature = "tray-icon"))))]
      pub fn tray_by_id<'a, I>(&self, id: &'a I) -> Option<TrayIcon<R>>
      where
        I: ?Sized,
        TrayIconId: PartialEq<&'a I>,
      {
        self.manager.tray.tray_by_id(id)
      }

      /// Removes a tray icon using the provided id from tauri's internal state and returns it.
      ///
      /// Note that dropping the returned icon, may cause the tray icon to disappear
      /// if it wasn't cloned somewhere else or referenced by JS.
      #[cfg(all(desktop, feature = "tray-icon"))]
      #[cfg_attr(docsrs, doc(cfg(all(desktop, feature = "tray-icon"))))]
      pub fn remove_tray_by_id<'a, I>(&self, id: &'a I) -> Option<TrayIcon<R>>
      where
        I: ?Sized,
        TrayIconId: PartialEq<&'a I>,
      {
        self.manager.tray.remove_tray_by_id(id)
      }

      /// Gets the app's configuration, defined on the `tauri.conf.json` file.
      pub fn config(&self) -> &Config {
        self.manager.config()
      }

      /// Gets the app's package information.
      pub fn package_info(&self) -> &PackageInfo {
        self.manager.package_info()
      }

      /// The application's asset resolver.
      pub fn asset_resolver(&self) -> AssetResolver<R> {
        AssetResolver {
          manager: self.manager.clone(),
        }
      }

      /// Returns the primary monitor of the system.
      ///
      /// Returns None if it can't identify any monitor as a primary one.
      pub fn primary_monitor(&self) -> crate::Result<Option<Monitor>> {
        Ok(match self.runtime() {
          RuntimeOrDispatch::Runtime(h) => h.primary_monitor().map(Into::into),
          RuntimeOrDispatch::RuntimeHandle(h) => h.primary_monitor().map(Into::into),
          _ => unreachable!(),
        })
      }

      /// Returns the monitor that contains the given point.
      pub fn monitor_from_point(&self, x: f64, y: f64) -> crate::Result<Option<Monitor>> {
        Ok(match self.runtime() {
          RuntimeOrDispatch::Runtime(h) => h.monitor_from_point(x, y).map(Into::into),
          RuntimeOrDispatch::RuntimeHandle(h) => h.monitor_from_point(x, y).map(Into::into),
          _ => unreachable!(),
        })
      }

      /// Returns the list of all the monitors available on the system.
      pub fn available_monitors(&self) -> crate::Result<Vec<Monitor>> {
        Ok(match self.runtime() {
          RuntimeOrDispatch::Runtime(h) => {
            h.available_monitors().into_iter().map(Into::into).collect()
          }
          RuntimeOrDispatch::RuntimeHandle(h) => {
            h.available_monitors().into_iter().map(Into::into).collect()
          }
          _ => unreachable!(),
        })
      }

      /// Get the cursor position relative to the top-left hand corner of the desktop.
      ///
      /// Note that the top-left hand corner of the desktop is not necessarily the same as the screen.
      /// If the user uses a desktop with multiple monitors,
      /// the top-left hand corner of the desktop is the top-left hand corner of the main monitor on Windows and macOS
      /// or the top-left of the leftmost monitor on X11.
      ///
      /// The coordinates can be negative if the top-left hand corner of the window is outside of the visible screen region.
      pub fn cursor_position(&self) -> crate::Result<PhysicalPosition<f64>> {
        Ok(match self.runtime() {
          RuntimeOrDispatch::Runtime(h) => h.cursor_position()?,
          RuntimeOrDispatch::RuntimeHandle(h) => h.cursor_position()?,
          _ => unreachable!(),
        })
      }

      /// Set the app theme.
      pub fn set_theme(&self, theme: Option<Theme>) {
        #[cfg(windows)]
        for window in self.manager.windows().values() {
          if let (Some(menu), Ok(hwnd)) = (window.menu(), window.hwnd()) {
            let raw_hwnd = hwnd.0 as isize;
            let _ = self.run_on_main_thread(move || {
              let _ = unsafe {
                menu.inner().set_theme_for_hwnd(
                  raw_hwnd,
                  theme
                    .map(crate::menu::map_to_menu_theme)
                    .unwrap_or(muda::MenuTheme::Auto),
                )
              };
            });
          };
        }
        match self.runtime() {
          RuntimeOrDispatch::Runtime(h) => h.set_theme(theme),
          RuntimeOrDispatch::RuntimeHandle(h) => h.set_theme(theme),
          _ => unreachable!(),
        }
      }

      /// Returns the default window icon.
      pub fn default_window_icon(&self) -> Option<&Image<'_>> {
        self.manager.window.default_icon.as_ref()
      }

      /// Returns the app-wide menu.
      #[cfg(desktop)]
      pub fn menu(&self) -> Option<Menu<R>> {
        self.manager.menu.menu_lock().clone()
      }

      /// Sets the app-wide menu and returns the previous one.
      ///
      /// If a window was not created with an explicit menu or had one set explicitly,
      /// this menu will be assigned to it.
      #[cfg(desktop)]
      pub fn set_menu(&self, menu: Menu<R>) -> crate::Result<Option<Menu<R>>> {
        let prev_menu = self.remove_menu()?;

        self.manager.menu.insert_menu_into_stash(&menu);

        self.manager.menu.menu_lock().replace(menu.clone());

        // set it on all windows that don't have one or previously had the app-wide menu
        #[cfg(not(target_os = "macos"))]
        {
          for window in self.manager.windows().values() {
            let has_app_wide_menu = window.has_app_wide_menu() || window.menu().is_none();
            if has_app_wide_menu {
              window.set_menu(menu.clone())?;
              window.menu_lock().replace(crate::window::WindowMenu {
                is_app_wide: true,
                menu: menu.clone(),
              });
            }
          }
        }

        // set it app-wide for macos
        #[cfg(target_os = "macos")]
        {
          let menu_ = menu.clone();
          self.run_on_main_thread(move || {
            let _ = init_app_menu(&menu_);
          })?;
        }

        Ok(prev_menu)
      }

      /// Remove the app-wide menu and returns it.
      ///
      /// If a window was not created with an explicit menu or had one set explicitly,
      /// this will remove the menu from it.
      #[cfg(desktop)]
      pub fn remove_menu(&self) -> crate::Result<Option<Menu<R>>> {
        let menu = self.manager.menu.menu_lock().as_ref().cloned();
        #[allow(unused_variables)]
        if let Some(menu) = menu {
          // remove from windows that have the app-wide menu
          #[cfg(not(target_os = "macos"))]
          {
            for window in self.manager.windows().values() {
              let has_app_wide_menu = window.has_app_wide_menu();
              if has_app_wide_menu {
                window.remove_menu()?;
                *window.menu_lock() = None;
              }
            }
          }

          // remove app-wide for macos
          #[cfg(target_os = "macos")]
          {
            self.run_on_main_thread(move || {
              menu.inner().remove_for_nsapp();
            })?;
          }
        }

        let prev_menu = self.manager.menu.menu_lock().take();

        self
          .manager
          .remove_menu_from_stash_by_id(prev_menu.as_ref().map(|m| m.id()));

        Ok(prev_menu)
      }

      /// Hides the app-wide menu from windows that have it.
      ///
      /// If a window was not created with an explicit menu or had one set explicitly,
      /// this will hide the menu from it.
      #[cfg(desktop)]
      pub fn hide_menu(&self) -> crate::Result<()> {
        #[cfg(not(target_os = "macos"))]
        {
          let is_app_menu_set = self.manager.menu.menu_lock().is_some();
          if is_app_menu_set {
            for window in self.manager.windows().values() {
              if window.has_app_wide_menu() {
                window.hide_menu()?;
              }
            }
          }
        }

        Ok(())
      }

      /// Shows the app-wide menu for windows that have it.
      ///
      /// If a window was not created with an explicit menu or had one set explicitly,
      /// this will show the menu for it.
      #[cfg(desktop)]
      pub fn show_menu(&self) -> crate::Result<()> {
        #[cfg(not(target_os = "macos"))]
        {
          let is_app_menu_set = self.manager.menu.menu_lock().is_some();
          if is_app_menu_set {
            for window in self.manager.windows().values() {
              if window.has_app_wide_menu() {
                window.show_menu()?;
              }
            }
          }
        }

        Ok(())
      }

      /// Shows the application, but does not automatically focus it.
      #[cfg(target_os = "macos")]
      pub fn show(&self) -> crate::Result<()> {
        match self.runtime() {
          RuntimeOrDispatch::Runtime(r) => r.show(),
          RuntimeOrDispatch::RuntimeHandle(h) => h.show()?,
          _ => unreachable!(),
        }
        Ok(())
      }

      /// Hides the application.
      #[cfg(target_os = "macos")]
      pub fn hide(&self) -> crate::Result<()> {
        match self.runtime() {
          RuntimeOrDispatch::Runtime(r) => r.hide(),
          RuntimeOrDispatch::RuntimeHandle(h) => h.hide()?,
          _ => unreachable!(),
        }
        Ok(())
      }

      /// Runs necessary cleanup tasks before exiting the process.
      /// **You should always exit the tauri app immediately after this function returns and not use any tauri-related APIs.**
      pub fn cleanup_before_exit(&self) {
        #[cfg(all(desktop, feature = "tray-icon"))]
        self.manager.tray.icons.lock().unwrap().clear();
        self.manager.resources_table().clear();
        for (_, window) in self.manager.windows() {
          window.resources_table().clear();
          #[cfg(windows)]
          let _ = window.hide();
        }
        for (_, webview) in self.manager.webviews() {
          webview.resources_table().clear();
        }
      }

      /// Gets the invoke key that must be referenced when using [`crate::webview::InvokeRequest`].
      ///
      /// # Security
      ///
      /// DO NOT expose this key to third party scripts as might grant access to the backend from external URLs and iframes.
      pub fn invoke_key(&self) -> &str {
        self.manager.invoke_key()
      }
    }

    impl<R: Runtime> Listener<R> for $app {
      /// Listen to an event on this app.
      ///
      /// # Examples
      ///
      /// ```
      /// use tauri::Listener;
      ///
      /// tauri::Builder::default()
      ///   .setup(|app| {
      ///     app.listen("component-loaded", move |event| {
      ///       println!("window just loaded a component");
      ///     });
      ///
      ///     Ok(())
      ///   });
      /// ```
      fn listen<F>(&self, event: impl Into<String>, handler: F) -> EventId
      where
        F: Fn(Event) + Send + 'static,
      {
        self.manager.listen(event.into(), EventTarget::App, handler)
      }

      /// Listen to an event on this app only once.
      ///
      /// See [`Self::listen`] for more information.
      fn once<F>(&self, event: impl Into<String>, handler: F) -> EventId
      where
        F: FnOnce(Event) + Send + 'static,
      {
        self.manager.once(event.into(), EventTarget::App, handler)
      }

      /// Unlisten to an event on this app.
      ///
      /// # Examples
      ///
      /// ```
      /// use tauri::Listener;
      ///
      /// tauri::Builder::default()
      ///   .setup(|app| {
      ///     let handler = app.listen("component-loaded", move |event| {
      ///       println!("app just loaded a component");
      ///     });
      ///
      ///     // stop listening to the event when you do not need it anymore
      ///     app.unlisten(handler);
      ///
      ///     Ok(())
      ///   });
      /// ```
      fn unlisten(&self, id: EventId) {
        self.manager.unlisten(id)
      }
    }

    impl<R: Runtime> Emitter<R> for $app {
      /// Emits an event to all [targets](EventTarget).
      ///
      /// # Examples
      /// ```
      /// use tauri::Emitter;
      ///
      /// #[tauri::command]
      /// fn synchronize(app: tauri::AppHandle) {
      ///   // emits the synchronized event to all webviews
      ///   app.emit("synchronized", ());
      /// }
      /// ```
      fn emit<S: Serialize + Clone>(&self, event: &str, payload: S) -> Result<()> {
        self.manager.emit(event, payload)
      }

      /// Emits an event to all [targets](EventTarget) matching the given target.
      ///
      /// # Examples
      /// ```
      /// use tauri::{Emitter, EventTarget};
      ///
      /// #[tauri::command]
      /// fn download(app: tauri::AppHandle) {
      ///   for i in 1..100 {
      ///     std::thread::sleep(std::time::Duration::from_millis(150));
      ///     // emit a download progress event to all listeners
      ///     app.emit_to(EventTarget::any(), "download-progress", i);
      ///     // emit an event to listeners that used App::listen or AppHandle::listen
      ///     app.emit_to(EventTarget::app(), "download-progress", i);
      ///     // emit an event to any webview/window/webviewWindow matching the given label
      ///     app.emit_to("updater", "download-progress", i); // similar to using EventTarget::labeled
      ///     app.emit_to(EventTarget::labeled("updater"), "download-progress", i);
      ///     // emit an event to listeners that used WebviewWindow::listen
      ///     app.emit_to(EventTarget::webview_window("updater"), "download-progress", i);
      ///   }
      /// }
      /// ```
      fn emit_to<I, S>(&self, target: I, event: &str, payload: S) -> Result<()>
      where
        I: Into<EventTarget>,
        S: Serialize + Clone,
      {
        self.manager.emit_to(target, event, payload)
      }

      /// Emits an event to all [targets](EventTarget) based on the given filter.
      ///
      /// # Examples
      /// ```
      /// use tauri::{Emitter, EventTarget};
      ///
      /// #[tauri::command]
      /// fn download(app: tauri::AppHandle) {
      ///   for i in 1..100 {
      ///     std::thread::sleep(std::time::Duration::from_millis(150));
      ///     // emit a download progress event to the updater window
      ///     app.emit_filter("download-progress", i, |t| match t {
      ///       EventTarget::WebviewWindow { label } => label == "main",
      ///       _ => false,
      ///     });
      ///   }
      /// }
      /// ```
      fn emit_filter<S, F>(&self, event: &str, payload: S, filter: F) -> Result<()>
      where
        S: Serialize + Clone,
        F: Fn(&EventTarget) -> bool,
      {
        self.manager.emit_filter(event, payload, filter)
      }
    }
  };
}

shared_app_impl!(App<R>);
shared_app_impl!(AppHandle<R>);

impl<R: Runtime> App<R> {
  #[cfg_attr(
    feature = "tracing",
    tracing::instrument(name = "app::core_plugins::register")
  )]
  fn register_core_plugins(&self) -> crate::Result<()> {
    self.handle.plugin(crate::path::plugin::init())?;
    self.handle.plugin(crate::event::plugin::init())?;
    self.handle.plugin(crate::window::plugin::init())?;
    self.handle.plugin(crate::webview::plugin::init())?;
    self.handle.plugin(crate::app::plugin::init())?;
    self.handle.plugin(crate::resources::plugin::init())?;
    self.handle.plugin(crate::image::plugin::init())?;
    #[cfg(desktop)]
    self.handle.plugin(crate::menu::plugin::init())?;
    #[cfg(all(desktop, feature = "tray-icon"))]
    self.handle.plugin(crate::tray::plugin::init())?;
    Ok(())
  }

  /// Runs the given closure on the main thread.
  pub fn run_on_main_thread<F: FnOnce() + Send + 'static>(&self, f: F) -> crate::Result<()> {
    self.app_handle().run_on_main_thread(f)
  }

  /// Gets a handle to the application instance.
  pub fn handle(&self) -> &AppHandle<R> {
    &self.handle
  }

  /// Sets the activation policy for the application. It is set to `NSApplicationActivationPolicyRegular` by default.
  ///
  /// # Examples
  /// ```,no_run
  /// tauri::Builder::default()
  ///   .setup(move |app| {
  ///     #[cfg(target_os = "macos")]
  ///     app.set_activation_policy(tauri::ActivationPolicy::Accessory);
  ///     Ok(())
  ///   });
  /// ```
  #[cfg(target_os = "macos")]
  #[cfg_attr(docsrs, doc(cfg(target_os = "macos")))]
  pub fn set_activation_policy(&mut self, activation_policy: ActivationPolicy) {
    if let Some(runtime) = self.runtime.as_mut() {
      runtime.set_activation_policy(activation_policy);
    } else {
      let _ = self.app_handle().set_activation_policy(activation_policy);
    }
  }

  /// Change the device event filter mode.
  ///
  /// Since the DeviceEvent capture can lead to high CPU usage for unfocused windows, [`tao`]
  /// will ignore them by default for unfocused windows on Windows. This method allows changing
  /// the filter to explicitly capture them again.
  ///
  /// ## Platform-specific
  ///
  /// - ** Linux / macOS / iOS / Android**: Unsupported.
  ///
  /// # Examples
  /// ```,no_run
  /// let mut app = tauri::Builder::default()
  ///   // on an actual app, remove the string argument
  ///   .build(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
  ///   .expect("error while building tauri application");
  /// app.set_device_event_filter(tauri::DeviceEventFilter::Always);
  /// app.run(|_app_handle, _event| {});
  /// ```
  ///
  /// [`tao`]: https://crates.io/crates/tao
  pub fn set_device_event_filter(&mut self, filter: DeviceEventFilter) {
    self
      .runtime
      .as_mut()
      .unwrap()
      .set_device_event_filter(filter);
  }

  /// Runs the application.
  ///
  /// # Examples
  /// ```,no_run
  /// let app = tauri::Builder::default()
  ///   // on an actual app, remove the string argument
  ///   .build(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
  ///   .expect("error while building tauri application");
  /// app.run(|_app_handle, event| match event {
  ///   tauri::RunEvent::ExitRequested { api, .. } => {
  ///     api.prevent_exit();
  ///   }
  ///   _ => {}
  /// });
  /// ```
  pub fn run<F: FnMut(&AppHandle<R>, RunEvent) + 'static>(mut self, mut callback: F) {
    let app_handle = self.handle().clone();
    let manager = self.manager.clone();
    self.runtime.take().unwrap().run(move |event| match event {
      RuntimeRunEvent::Ready => {
        if let Err(e) = setup(&mut self) {
          panic!("Failed to setup app: {e}");
        }
        let event = on_event_loop_event(&app_handle, RuntimeRunEvent::Ready, &manager);
        callback(&app_handle, event);
      }
      RuntimeRunEvent::Exit => {
        let event = on_event_loop_event(&app_handle, RuntimeRunEvent::Exit, &manager);
        callback(&app_handle, event);
        app_handle.cleanup_before_exit();
      }
      _ => {
        let event = on_event_loop_event(&app_handle, event, &manager);
        callback(&app_handle, event);
      }
    });
  }

  /// Runs an iteration of the runtime event loop and immediately return.
  ///
  /// Note that when using this API, app cleanup is not automatically done.
  /// The cleanup calls [`App::cleanup_before_exit`] so you may want to call that function before exiting the application.
  ///
  /// # Examples
  /// ```no_run
  /// use tauri::Manager;
  ///
  /// let mut app = tauri::Builder::default()
  ///   // on an actual app, remove the string argument
  ///   .build(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
  ///   .expect("error while building tauri application");
  ///
  /// loop {
  ///   app.run_iteration(|_app, _event| {});
  ///   if app.webview_windows().is_empty() {
  ///     app.cleanup_before_exit();
  ///     break;
  ///   }
  /// }
  /// ```
  #[cfg(desktop)]
  pub fn run_iteration<F: FnMut(&AppHandle<R>, RunEvent) + 'static>(&mut self, mut callback: F) {
    let manager = self.manager.clone();
    let app_handle = self.handle().clone();

    if !self.ran_setup {
      if let Err(e) = setup(self) {
        panic!("Failed to setup app: {e}");
      }
    }

    self.runtime.as_mut().unwrap().run_iteration(move |event| {
      let event = on_event_loop_event(&app_handle, event, &manager);
      callback(&app_handle, event);
    })
  }
}

/// Builds a Tauri application.
///
/// # Examples
/// ```,no_run
/// tauri::Builder::default()
///   // on an actual app, remove the string argument
///   .run(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
///  .expect("error while running tauri application");
/// ```
#[allow(clippy::type_complexity)]
pub struct Builder<R: Runtime> {
  /// A flag indicating that the runtime must be started on an environment that supports the event loop not on the main thread.
  #[cfg(any(windows, target_os = "linux"))]
  runtime_any_thread: bool,

  /// The JS message handler.
  invoke_handler: Box<InvokeHandler<R>>,

  /// The script that initializes the `window.__TAURI_INTERNALS__.postMessage` function.
  pub(crate) invoke_initialization_script: String,

  /// The setup hook.
  setup: SetupHook<R>,

  /// Page load hook.
  on_page_load: Option<Arc<OnPageLoad<R>>>,

  /// All passed plugins
  plugins: PluginStore<R>,

  /// The webview protocols available to all windows.
  uri_scheme_protocols: HashMap<String, Arc<UriSchemeProtocol<R>>>,

  /// App state.
  state: StateManager,

  /// A closure that returns the menu set to all windows.
  #[cfg(desktop)]
  menu: Option<Box<dyn FnOnce(&AppHandle<R>) -> crate::Result<Menu<R>> + Send>>,

  /// Menu event listeners for any menu event.
  #[cfg(desktop)]
  menu_event_listeners: Vec<GlobalMenuEventListener<AppHandle<R>>>,

  /// Enable macOS default menu creation.
  #[allow(unused)]
  enable_macos_default_menu: bool,

  /// Window event handlers that listens to all windows.
  window_event_listeners: Vec<GlobalWindowEventListener<R>>,

  /// Webview event handlers that listens to all webviews.
  webview_event_listeners: Vec<GlobalWebviewEventListener<R>>,

  /// The device event filter.
  device_event_filter: DeviceEventFilter,

  pub(crate) invoke_key: String,
}

#[derive(Template)]
#[default_template("../scripts/ipc-protocol.js")]
pub(crate) struct InvokeInitializationScript<'a> {
  /// The function that processes the IPC message.
  #[raw]
  pub(crate) process_ipc_message_fn: &'a str,
  pub(crate) os_name: &'a str,
  pub(crate) fetch_channel_data_command: &'a str,
  pub(crate) invoke_key: &'a str,
}

/// Make `Wry` the default `Runtime` for `Builder`
#[cfg(feature = "wry")]
#[cfg_attr(docsrs, doc(cfg(feature = "wry")))]
impl Default for Builder<crate::Wry> {
  fn default() -> Self {
    Self::new()
  }
}

#[cfg(not(feature = "wry"))]
#[cfg_attr(docsrs, doc(cfg(not(feature = "wry"))))]
impl<R: Runtime> Default for Builder<R> {
  fn default() -> Self {
    Self::new()
  }
}

impl<R: Runtime> Builder<R> {
  /// Creates a new App builder.
  pub fn new() -> Self {
    let invoke_key = crate::generate_invoke_key().unwrap();

    Self {
      #[cfg(any(windows, target_os = "linux"))]
      runtime_any_thread: false,
      setup: Box::new(|_| Ok(())),
      invoke_handler: Box::new(|_| false),
      invoke_initialization_script: InvokeInitializationScript {
        process_ipc_message_fn: crate::manager::webview::PROCESS_IPC_MESSAGE_FN,
        os_name: std::env::consts::OS,
        fetch_channel_data_command: crate::ipc::channel::FETCH_CHANNEL_DATA_COMMAND,
        invoke_key: &invoke_key.clone(),
      }
      .render_default(&Default::default())
      .unwrap()
      .into_string(),
      on_page_load: None,
      plugins: PluginStore::default(),
      uri_scheme_protocols: Default::default(),
      state: StateManager::new(),
      #[cfg(desktop)]
      menu: None,
      #[cfg(desktop)]
      menu_event_listeners: Vec::new(),
      enable_macos_default_menu: true,
      window_event_listeners: Vec::new(),
      webview_event_listeners: Vec::new(),
      device_event_filter: Default::default(),
      invoke_key,
    }
  }
}

impl<R: Runtime> Builder<R> {
  /// Builds a new Tauri application running on any thread, bypassing the main thread requirement.
  ///
  /// ## Platform-specific
  ///
  /// - **macOS:** on macOS the application *must* be executed on the main thread, so this function is not exposed.
  #[cfg(any(windows, target_os = "linux"))]
  #[cfg_attr(docsrs, doc(cfg(any(windows, target_os = "linux"))))]
  #[must_use]
  pub fn any_thread(mut self) -> Self {
    self.runtime_any_thread = true;
    self
  }

  /// Defines the JS message handler callback.
  ///
  /// # Examples
  /// ```
  /// #[tauri::command]
  /// fn command_1() -> String {
  ///   return "hello world".to_string();
  /// }
  /// tauri::Builder::default()
  ///   .invoke_handler(tauri::generate_handler![
  ///     command_1,
  ///     // etc...
  ///   ]);
  /// ```
  #[must_use]
  pub fn invoke_handler<F>(mut self, invoke_handler: F) -> Self
  where
    F: Fn(Invoke<R>) -> bool + Send + Sync + 'static,
  {
    self.invoke_handler = Box::new(invoke_handler);
    self
  }

  /// Defines a custom JS message system.
  ///
  /// The `initialization_script` is a script that initializes `window.__TAURI_INTERNALS__.postMessage`.
  /// That function must take the `(message: object, options: object)` arguments and send it to the backend.
  ///
  /// Additionally, the script must include a `__INVOKE_KEY__` token that is replaced with a value that must be sent with the IPC payload
  /// to check the integrity of the message by the [`crate::WebviewWindow::on_message`] API, e.g.
  ///
  /// ```js
  /// const invokeKey = __INVOKE_KEY__;
  /// fetch('my-impl://command', {
  ///   headers: {
  ///     'Tauri-Invoke-Key': invokeKey,
  ///   }
  /// })
  /// ```
  ///
  /// Note that the implementation details is up to your implementation.
  #[must_use]
  pub fn invoke_system(mut self, initialization_script: String) -> Self {
    self.invoke_initialization_script =
      initialization_script.replace("__INVOKE_KEY__", &format!("\"{}\"", self.invoke_key));
    self
  }

  /// Append a custom initialization script.
  ///
  /// Allow to append custom initialization script instend of replacing entire invoke system.
  ///
  /// # Examples
  ///
  /// ```
  /// let custom_script = r#"
  /// // A custom call system bridge build on top of tauri invoke system.
  /// async function invoke(cmd, args = {}) {
  ///   if (!args) args = {};
  ///
  ///   let prefix = "";
  ///
  ///   if (args?.__module) {
  ///     prefix = `plugin:hybridcall.${args.__module}|`;
  ///   }
  ///
  ///   const command = `${prefix}tauri_${cmd}`;
  ///
  ///   const invoke = window.__TAURI_INTERNALS__.invoke;
  ///
  ///   return invoke(command, args).then(result => {
  ///     if (window.build.debug) {
  ///       console.log(`call: ${command}`);
  ///       console.log(`args: ${JSON.stringify(args)}`);
  ///       console.log(`return: ${JSON.stringify(result)}`);
  ///     }
  ///
  ///     return result;
  ///   });
  /// }
  /// "#;
  ///
  /// tauri::Builder::default()
  ///   .append_invoke_initialization_script(custom_script);
  /// ```
  pub fn append_invoke_initialization_script(
    mut self,
    initialization_script: impl AsRef<str>,
  ) -> Self {
    self
      .invoke_initialization_script
      .push_str(initialization_script.as_ref());
    self
  }

  /// Defines the setup hook.
  ///
  /// # Examples
  #[cfg_attr(
    feature = "unstable",
    doc = r####"
```
use tauri::Manager;
tauri::Builder::default()
  .setup(|app| {
    let main_window = app.get_window("main").unwrap();
    main_window.set_title("Tauri!")?;
    Ok(())
  });
```
  "####
  )]
  #[must_use]
  pub fn setup<F>(mut self, setup: F) -> Self
  where
    F: FnOnce(&mut App<R>) -> std::result::Result<(), Box<dyn std::error::Error>> + Send + 'static,
  {
    self.setup = Box::new(setup);
    self
  }

  /// Defines the page load hook.
  #[must_use]
  pub fn on_page_load<F>(mut self, on_page_load: F) -> Self
  where
    F: Fn(&Webview<R>, &PageLoadPayload<'_>) + Send + Sync + 'static,
  {
    self.on_page_load.replace(Arc::new(on_page_load));
    self
  }

  /// Adds a Tauri application plugin.
  ///
  /// A plugin is created using the [`crate::plugin::Builder`] struct.Check its documentation for more information.
  ///
  /// # Examples
  ///
  /// ```
  /// mod plugin {
  ///   use tauri::{plugin::{Builder as PluginBuilder, TauriPlugin}, RunEvent, Runtime};
  ///
  ///   // this command can be called in the frontend using `invoke('plugin:window|do_something')`.
  ///   #[tauri::command]
  ///   async fn do_something<R: Runtime>(app: tauri::AppHandle<R>, window: tauri::Window<R>) -> Result<(), String> {
  ///     println!("command called");
  ///     Ok(())
  ///   }
  ///   pub fn init<R: Runtime>() -> TauriPlugin<R> {
  ///     PluginBuilder::new("window")
  ///       .setup(|app, api| {
  ///         // initialize the plugin here
  ///         Ok(())
  ///       })
  ///       .on_event(|app, event| {
  ///         match event {
  ///           RunEvent::Ready => {
  ///             println!("app is ready");
  ///           }
  ///           RunEvent::WindowEvent { label, event, .. } => {
  ///             println!("window {} received an event: {:?}", label, event);
  ///           }
  ///           _ => (),
  ///         }
  ///       })
  ///       .invoke_handler(tauri::generate_handler![do_something])
  ///       .build()
  ///   }
  /// }
  ///
  /// tauri::Builder::default()
  ///   .plugin(plugin::init());
  /// ```
  #[must_use]
  pub fn plugin<P: Plugin<R> + 'static>(mut self, plugin: P) -> Self {
    self.plugins.register(Box::new(plugin));
    self
  }

  /// Add `state` to the state managed by the application.
  ///
  /// This method can be called any number of times as long as each call
  /// refers to a different `T`.
  ///
  /// Managed state can be retrieved by any command handler via the
  /// [`crate::State`] guard. In particular, if a value of type `T`
  /// is managed by Tauri, adding `State<T>` to the list of arguments in a
  /// command handler instructs Tauri to retrieve the managed value.
  /// Additionally, [`state`](crate::Manager#method.state) can be used to retrieve the value manually.
  ///
  /// # Panics
  ///
  /// Panics if state of type `T` is already being managed.
  ///
  /// # Mutability
  ///
  /// Since the managed state is global and must be [`Send`] + [`Sync`], mutations can only happen through interior mutability:
  ///
  /// ```,no_run
  /// use std::{collections::HashMap, sync::Mutex};
  /// use tauri::State;
  /// // here we use Mutex to achieve interior mutability
  /// struct Storage {
  ///   store: Mutex<HashMap<u64, String>>,
  /// }
  /// struct Connection;
  /// struct DbConnection {
  ///   db: Mutex<Option<Connection>>,
  /// }
  ///
  /// #[tauri::command]
  /// fn connect(connection: State<DbConnection>) {
  ///   // initialize the connection, mutating the state with interior mutability
  ///   *connection.db.lock().unwrap() = Some(Connection {});
  /// }
  ///
  /// #[tauri::command]
  /// fn storage_insert(key: u64, value: String, storage: State<Storage>) {
  ///   // mutate the storage behind the Mutex
  ///   storage.store.lock().unwrap().insert(key, value);
  /// }
  ///
  /// tauri::Builder::default()
  ///   .manage(Storage { store: Default::default() })
  ///   .manage(DbConnection { db: Default::default() })
  ///   .invoke_handler(tauri::generate_handler![connect, storage_insert])
  ///   // on an actual app, remove the string argument
  ///   .run(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
  ///   .expect("error while running tauri application");
  /// ```
  ///
  /// # Examples
  ///
  /// ```,no_run
  /// use tauri::State;
  ///
  /// struct MyInt(isize);
  /// struct MyString(String);
  ///
  /// #[tauri::command]
  /// fn int_command(state: State<MyInt>) -> String {
  ///     format!("The stateful int is: {}", state.0)
  /// }
  ///
  /// #[tauri::command]
  /// fn string_command<'r>(state: State<'r, MyString>) {
  ///     println!("state: {}", state.inner().0);
  /// }
  ///
  /// tauri::Builder::default()
  ///   .manage(MyInt(10))
  ///   .manage(MyString("Hello, managed state!".to_string()))
  ///   .invoke_handler(tauri::generate_handler![int_command, string_command])
  ///   // on an actual app, remove the string argument
  ///   .run(tauri::generate_context!("test/fixture/src-tauri/tauri.conf.json"))
  ///   .expect("error while running tauri application");
  /// ```
  #[must_use]
  pub fn manage<T>(self, state: T) -> Self
  where
    T: Send + Sync + 'static,
  {
    let type_name = std::any::type_name::<T>();
    assert!(
      self.state.set(state),
      "state for type '{type_name}' is already being managed",
    );
    self
  }

  /// Sets the menu to use on all windows.
  ///
  /// # Examples
  /// ```
  /// use tauri::menu::{Menu, MenuItem, PredefinedMenuItem, Submenu};
  ///
  /// tauri::Builder::default()
  ///   .menu(|handle| Menu::with_items(handle, &[
  ///     &Submenu::with_items(
  ///       handle,
  ///       "File",
  ///       true,
  ///       &[
  ///         &PredefinedMenuItem::close_window(handle, None)?,
  ///         #[cfg(target_os = "macos")]
  ///         &MenuItem::new(handle, "Hello", true, None::<&str>)?,
  ///       ],
  ///     )?
  ///   ]));
  /// ```
  #[must_use]
  #[cfg(desktop)]
  pub fn menu<F: FnOnce(&AppHandle<R>) -> crate::Result<Menu<R>> + Send + 'static>(
    mut self,
    f: F,
  ) -> Self {
    self.menu.replace(Box::new(f));
    self
  }

  /// Registers an event handler for any menu event.
  ///
  /// # Examples
  /// ```
  /// use tauri::menu::*;
  ///
  /// tauri::Builder::default()
  ///   .on_menu_event(|app, event| {
  ///      if event.id() == "quit" {
  ///        app.exit(0);
  ///      }
  ///   });
  /// ```
  #[must_use]
  #[cfg(desktop)]
  pub fn on_menu_event<F: Fn(&AppHandle<R>, MenuEvent) + Send + Sync + 'static>(
    mut self,
    f: F,
  ) -> Self {
    self.menu_event_listeners.push(Box::new(f));
    self
  }

  /// Enable or disable the default menu on macOS. Enabled by default.
  ///
  /// # Examples
  /// ```
  /// tauri::Builder::default()
  ///   .enable_macos_default_menu(false);
  /// ```
  #[must_use]
  pub fn enable_macos_default_menu(mut self, enable: bool) -> Self {
    self.enable_macos_default_menu = enable;
    self
  }

  /// Registers a window event handler for all windows.
  ///
  /// # Examples
  /// ```
  /// tauri::Builder::default()
  ///   .on_window_event(|window, event| match event {
  ///     tauri::WindowEvent::Focused(focused) => {
  ///       // hide window whenever it loses focus
  ///       if !focused {
  ///         window.hide().unwrap();
  ///       }
  ///     }
  ///     _ => {}
  ///   });
  /// ```
  #[must_use]
  pub fn on_window_event<F: Fn(&Window<R>, &WindowEvent) + Send + Sync + 'static>(
    mut self,
    handler: F,
  ) -> Self {
    self.window_event_listeners.push(Box::new(handler));
    self
  }

  /// Registers a webview event handler for all webviews.
  ///
  /// # Examples
  /// ```
  /// tauri::Builder::default()
  ///   .on_webview_event(|window, event| match event {
  ///     tauri::WebviewEvent::DragDrop(event) => {
  ///       println!("{:?}", event);
  ///     }
  ///     _ => {}
  ///   });
  /// ```
  #[must_use]
  pub fn on_webview_event<F: Fn(&Webview<R>, &WebviewEvent) + Send + Sync + 'static>(
    mut self,
    handler: F,
  ) -> Self {
    self.webview_event_listeners.push(Box::new(handler));
    self
  }

  /// Registers a URI scheme protocol available to all webviews.
  ///
  /// Leverages [setURLSchemeHandler](https://developer.apple.com/documentation/webkit/wkwebviewconfiguration/2875766-seturlschemehandler) on macOS,
  /// [AddWebResourceRequestedFilter](https://docs.microsoft.com/en-us/dotnet/api/microsoft.web.webview2.core.corewebview2.addwebresourcerequestedfilter?view=webview2-dotnet-1.0.774.44) on Windows
  /// and [webkit-web-context-register-uri-scheme](https://webkitgtk.org/reference/webkit2gtk/stable/WebKitWebContext.html#webkit-web-context-register-uri-scheme) on Linux.
  ///
  /// # Arguments
  ///
  /// * `uri_scheme` The URI scheme to register, such as `example`.
  /// * `protocol` the protocol associated with the given URI scheme. It's a function that takes a request and returns a response.
  ///
  /// # Examples
  /// ```
  /// tauri::Builder::default()
  ///   .register_uri_scheme_protocol("app-files", |_ctx, request| {
  ///     // skip leading `/`
  ///     if let Ok(data) = std::fs::read(&request.uri().path()[1..]) {
  ///       http::Response::builder()
  ///         .body(data)
  ///         .unwrap()
  ///     } else {
  ///       http::Response::builder()
  ///         .status(http::StatusCode::BAD_REQUEST)
  ///         .header(http::header::CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())
  ///         .body("failed to read file".as_bytes().to_vec())
  ///         .unwrap()
  ///     }
  ///   });
  /// ```
  #[must_use]
  pub fn register_uri_scheme_protocol<
    N: Into<String>,
    T: Into<Cow<'static, [u8]>>,
    H: Fn(UriSchemeContext<'_, R>, http::Request<Vec<u8>>) -> http::Response<T>
      + Send
      + Sync
      + 'static,
  >(
    mut self,
    uri_scheme: N,
    protocol: H,
  ) -> Self {
    self.uri_scheme_protocols.insert(
      uri_scheme.into(),
      Arc::new(UriSchemeProtocol {
        protocol: Box::new(move |ctx, request, responder| {
          responder.respond(protocol(ctx, request))
        }),
      }),
    );
    self
  }

  /// Similar to [`Self::register_uri_scheme_protocol`] but with an asynchronous responder that allows you
  /// to process the request in a separate thread and respond asynchronously.
  ///
  /// # Arguments
  ///
  /// * `uri_scheme` The URI scheme to register, such as `example`.
  /// * `protocol` the protocol associated with the given URI scheme. It's a function that takes an URL such as `example://localhost/asset.css`.
  ///
  /// # Examples
  /// ```
  /// tauri::Builder::default()
  ///   .register_asynchronous_uri_scheme_protocol("app-files", |_ctx, request, responder| {
  ///     // skip leading `/`
  ///     let path = request.uri().path()[1..].to_string();
  ///     std::thread::spawn(move || {
  ///       if let Ok(data) = std::fs::read(path) {
  ///         responder.respond(
  ///           http::Response::builder()
  ///             .body(data)
  ///             .unwrap()
  ///         );
  ///       } else {
  ///         responder.respond(
  ///           http::Response::builder()
  ///             .status(http::StatusCode::BAD_REQUEST)
  ///             .header(http::header::CONTENT_TYPE, mime::TEXT_PLAIN.essence_str())
  ///             .body("failed to read file".as_bytes().to_vec())
  ///             .unwrap()
  ///         );
  ///     }
  ///   });
  ///   });
  /// ```
  #[must_use]
  pub fn register_asynchronous_uri_scheme_protocol<
    N: Into<String>,
    H: Fn(UriSchemeContext<'_, R>, http::Request<Vec<u8>>, UriSchemeResponder) + Send + Sync + 'static,
  >(
    mut self,
    uri_scheme: N,
    protocol: H,
  ) -> Self {
    self.uri_scheme_protocols.insert(
      uri_scheme.into(),
      Arc::new(UriSchemeProtocol {
        protocol: Box::new(protocol),
      }),
    );
    self
  }

  /// Change the device event filter mode.
  ///
  /// Since the DeviceEvent capture can lead to high CPU usage for unfocused windows, [`tao`]
  /// will ignore them by default for unfocused windows on Windows. This method allows changing
  /// the filter to explicitly capture them again.
  ///
  /// ## Platform-specific
  ///
  /// - ** Linux / macOS / iOS / Android**: Unsupported.
  ///
  /// # Examples
  /// ```,no_run
  /// tauri::Builder::default()
  ///   .device_event_filter(tauri::DeviceEventFilter::Always);
  /// ```
  ///
  /// [`tao`]: https://crates.io/crates/tao
  pub fn device_event_filter(mut self, filter: DeviceEventFilter) -> Self {
    self.device_event_filter = filter;
    self
  }

  /// Builds the application.
  #[allow(clippy::type_complexity, unused_mut)]
  #[cfg_attr(
    feature = "tracing",
    tracing::instrument(name = "app::build", skip_all)
  )]
  pub fn build(mut self, context: Context<R>) -> crate::Result<App<R>> {
    #[cfg(target_os = "macos")]
    if self.menu.is_none() && self.enable_macos_default_menu {
      self.menu = Some(Box::new(|app_handle| {
        crate::menu::Menu::default(app_handle)
      }));
    }

    let manager = Arc::new(AppManager::with_handlers(
      context,
      self.plugins,
      self.invoke_handler,
      self.on_page_load,
      self.uri_scheme_protocols,
      self.state,
      #[cfg(desktop)]
      self.menu_event_listeners,
      self.window_event_listeners,
      self.webview_event_listeners,
      #[cfg(desktop)]
      HashMap::new(),
      self.invoke_initialization_script,
      self.invoke_key,
    ));

    #[cfg(any(
      target_os = "linux",
      target_os = "dragonfly",
      target_os = "freebsd",
      target_os = "netbsd",
      target_os = "openbsd"
    ))]
    let app_id = if manager.config.app.enable_gtk_app_id {
      Some(manager.config.identifier.clone())
    } else {
      None
    };

    let runtime_args = RuntimeInitArgs {
      #[cfg(any(
        target_os = "linux",
        target_os = "dragonfly",
        target_os = "freebsd",
        target_os = "netbsd",
        target_os = "openbsd"
      ))]
      app_id,

      #[cfg(windows)]
      msg_hook: {
        let menus = manager.menu.menus.clone();
        Some(Box::new(move |msg| {
          use windows::Win32::UI::WindowsAndMessaging::{TranslateAcceleratorW, HACCEL, MSG};
          unsafe {
            let msg = msg as *const MSG;
            for menu in menus.lock().unwrap().values() {
              let translated =
                TranslateAcceleratorW((*msg).hwnd, HACCEL(menu.inner().haccel() as _), msg);
              if translated == 1 {
                return true;
              }
            }

            false
          }
        }))
      },
    };

    #[cfg(any(windows, target_os = "linux"))]
    let mut runtime = if self.runtime_any_thread {
      R::new_any_thread(runtime_args)?
    } else {
      R::new(runtime_args)?
    };
    #[cfg(not(any(windows, target_os = "linux")))]
    let mut runtime = R::new(runtime_args)?;

    #[cfg(desktop)]
    {
      // setup menu event handler
      let proxy = runtime.create_proxy();
      muda::MenuEvent::set_event_handler(Some(move |e: muda::MenuEvent| {
        let _ = proxy.send_event(EventLoopMessage::MenuEvent(e.into()));
      }));

      // setup tray event handler
      #[cfg(feature = "tray-icon")]
      {
        let proxy = runtime.create_proxy();
        tray_icon::TrayIconEvent::set_event_handler(Some(move |e: tray_icon::TrayIconEvent| {
          let _ = proxy.send_event(EventLoopMessage::TrayIconEvent(e.into()));
        }));
      }
    }

    runtime.set_device_event_filter(self.device_event_filter);

    let runtime_handle = runtime.handle();

    #[allow(unused_mut)]
    let mut app = App {
      runtime: Some(runtime),
      setup: Some(self.setup),
      manager: manager.clone(),
      handle: AppHandle {
        runtime_handle,
        manager,
      },
      ran_setup: false,
    };

    #[cfg(desktop)]
    if let Some(menu) = self.menu {
      let menu = menu(&app.handle)?;
      app
        .manager
        .menu
        .menus_stash_lock()
        .insert(menu.id().clone(), menu.clone());

      #[cfg(target_os = "macos")]
      init_app_menu(&menu)?;

      app.manager.menu.menu_lock().replace(menu);
    }

    app.register_core_plugins()?;

    let env = Env::default();
    app.manage(env);

    app.manage(Scopes {
      #[cfg(feature = "protocol-asset")]
      asset_protocol: crate::scope::fs::Scope::new(
        &app,
        &app.config().app.security.asset_protocol.scope,
      )?,
    });

    app.manage(ChannelDataIpcQueue::default());
    app.handle.plugin(crate::ipc::channel::plugin())?;

    #[cfg(windows)]
    {
      if let crate::utils::config::WebviewInstallMode::FixedRuntime { path } =
        &app.manager.config().bundle.windows.webview_install_mode
      {
        if let Ok(resource_dir) = app.path().resource_dir() {
          std::env::set_var(
            "WEBVIEW2_BROWSER_EXECUTABLE_FOLDER",
            resource_dir.join(path),
          );
        } else {
          #[cfg(debug_assertions)]
          eprintln!(
            "failed to resolve resource directory; fallback to the installed Webview2 runtime."
          );
        }
      }
    }

    let handle = app.handle();

    // initialize default tray icon if defined
    #[cfg(all(desktop, feature = "tray-icon"))]
    {
      let config = app.config();
      if let Some(tray_config) = &config.app.tray_icon {
        let mut tray =
          TrayIconBuilder::with_id(tray_config.id.clone().unwrap_or_else(|| "main".into()))
            .icon_as_template(tray_config.icon_as_template)
            .menu_on_left_click(tray_config.menu_on_left_click);
        if let Some(icon) = &app.manager.tray.icon {
          tray = tray.icon(icon.clone());
        }
        if let Some(title) = &tray_config.title {
          tray = tray.title(title);
        }
        if let Some(tooltip) = &tray_config.tooltip {
          tray = tray.tooltip(tooltip);
        }
        tray.build(handle)?;
      }
    }

    app.manager.initialize_plugins(handle)?;

    Ok(app)
  }

  /// Runs the configured Tauri application.
  pub fn run(self, context: Context<R>) -> crate::Result<()> {
    self.build(context)?.run(|_, _| {});
    Ok(())
  }
}

pub(crate) type UriSchemeResponderFn = Box<dyn FnOnce(http::Response<Cow<'static, [u8]>>) + Send>;

/// Async uri scheme protocol responder.
pub struct UriSchemeResponder(pub(crate) UriSchemeResponderFn);

impl UriSchemeResponder {
  /// Resolves the request with the given response.
  pub fn respond<T: Into<Cow<'static, [u8]>>>(self, response: http::Response<T>) {
    let (parts, body) = response.into_parts();
    (self.0)(http::Response::from_parts(parts, body.into()))
  }
}

/// Uri scheme protocol context
pub struct UriSchemeContext<'a, R: Runtime> {
  pub(crate) app_handle: &'a AppHandle<R>,
  pub(crate) webview_label: &'a str,
}

impl<'a, R: Runtime> UriSchemeContext<'a, R> {
  /// Get a reference to an [`AppHandle`].
  pub fn app_handle(&self) -> &'a AppHandle<R> {
    self.app_handle
  }

  /// Get the webview label that made the uri scheme request.
  pub fn webview_label(&self) -> &'a str {
    self.webview_label
  }
}

#[cfg(target_os = "macos")]
fn init_app_menu<R: Runtime>(menu: &Menu<R>) -> crate::Result<()> {
  menu.inner().init_for_nsapp();

  if let Some(window_menu) = menu.get(crate::menu::WINDOW_SUBMENU_ID) {
    if let Some(m) = window_menu.as_submenu() {
      m.set_as_windows_menu_for_nsapp()?;
    }
  }
  if let Some(help_menu) = menu.get(crate::menu::HELP_SUBMENU_ID) {
    if let Some(m) = help_menu.as_submenu() {
      m.set_as_help_menu_for_nsapp()?;
    }
  }

  Ok(())
}

impl<R: Runtime> raw_window_handle::HasDisplayHandle for AppHandle<R> {
  fn display_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
    self.runtime_handle.display_handle()
  }
}

impl<R: Runtime> raw_window_handle::HasDisplayHandle for App<R> {
  fn display_handle(
    &self,
  ) -> std::result::Result<raw_window_handle::DisplayHandle<'_>, raw_window_handle::HandleError> {
    self.handle.display_handle()
  }
}

unsafe impl<R: Runtime> raw_window_handle5::HasRawDisplayHandle for AppHandle<R> {
  fn raw_display_handle(&self) -> raw_window_handle5::RawDisplayHandle {
    self.runtime_handle.raw_display_handle()
  }
}

unsafe impl<R: Runtime> raw_window_handle5::HasRawDisplayHandle for App<R> {
  fn raw_display_handle(&self) -> raw_window_handle5::RawDisplayHandle {
    self.handle.raw_display_handle()
  }
}

#[cfg_attr(feature = "tracing", tracing::instrument(name = "app::setup"))]
fn setup<R: Runtime>(app: &mut App<R>) -> crate::Result<()> {
  app.ran_setup = true;

  for window_config in app.config().app.windows.iter().filter(|w| w.create) {
    WebviewWindowBuilder::from_config(app.handle(), window_config)?.build()?;
  }

  app.manager.assets.setup(app);

  if let Some(setup) = app.setup.take() {
    (setup)(app).map_err(|e| crate::Error::Setup(e.into()))?;
  }

  Ok(())
}

fn on_event_loop_event<R: Runtime>(
  app_handle: &AppHandle<R>,
  event: RuntimeRunEvent<EventLoopMessage>,
  manager: &AppManager<R>,
) -> RunEvent {
  if let RuntimeRunEvent::WindowEvent {
    label,
    event: RuntimeWindowEvent::Destroyed,
  } = &event
  {
    manager.on_window_close(label);
  }

  let event = match event {
    RuntimeRunEvent::Exit => RunEvent::Exit,
    RuntimeRunEvent::ExitRequested { code, tx } => RunEvent::ExitRequested {
      code,
      api: ExitRequestApi(tx),
    },
    RuntimeRunEvent::WindowEvent { label, event } => RunEvent::WindowEvent {
      label,
      event: event.into(),
    },
    RuntimeRunEvent::WebviewEvent { label, event } => RunEvent::WebviewEvent {
      label,
      event: event.into(),
    },
    RuntimeRunEvent::Ready => {
      // set the app icon in development
      #[cfg(all(dev, target_os = "macos"))]
      {
        use objc2::ClassType;
        use objc2_app_kit::{NSApplication, NSImage};
        use objc2_foundation::{MainThreadMarker, NSData};

        if let Some(icon) = app_handle.manager.app_icon.clone() {
          // TODO: Enable this check.
          let mtm = unsafe { MainThreadMarker::new_unchecked() };
          let app = NSApplication::sharedApplication(mtm);
          let data = NSData::with_bytes(&icon);
          let app_icon = NSImage::initWithData(NSImage::alloc(), &data).expect("creating icon");
          unsafe { app.setApplicationIconImage(Some(&app_icon)) };
        }
      }
      RunEvent::Ready
    }
    RuntimeRunEvent::Resumed => RunEvent::Resumed,
    RuntimeRunEvent::MainEventsCleared => RunEvent::MainEventsCleared,
    RuntimeRunEvent::UserEvent(t) => {
      match t {
        #[cfg(desktop)]
        EventLoopMessage::MenuEvent(ref e) => {
          for listener in &*app_handle
            .manager
            .menu
            .global_event_listeners
            .lock()
            .unwrap()
          {
            listener(app_handle, e.clone());
          }
          for (label, listener) in &*app_handle.manager.menu.event_listeners.lock().unwrap() {
            if let Some(w) = app_handle.manager().get_window(label) {
              listener(&w, e.clone());
            }
          }
        }
        #[cfg(all(desktop, feature = "tray-icon"))]
        EventLoopMessage::TrayIconEvent(ref e) => {
          for listener in &*app_handle
            .manager
            .tray
            .global_event_listeners
            .lock()
            .unwrap()
          {
            listener(app_handle, e.clone());
          }

          for (id, listener) in &*app_handle.manager.tray.event_listeners.lock().unwrap() {
            if e.id() == id {
              if let Some(tray) = app_handle.tray_by_id(id) {
                listener(&tray, e.clone());
              }
            }
          }
        }
      }

      #[allow(unreachable_code)]
      t.into()
    }
    #[cfg(any(target_os = "macos", target_os = "ios"))]
    RuntimeRunEvent::Opened { urls } => RunEvent::Opened { urls },
    #[cfg(target_os = "macos")]
    RuntimeRunEvent::Reopen {
      has_visible_windows,
    } => RunEvent::Reopen {
      has_visible_windows,
    },
    _ => unimplemented!(),
  };

  manager
    .plugins
    .lock()
    .expect("poisoned plugin store")
    .on_event(app_handle, &event);

  event
}

#[cfg(test)]
mod tests {
  #[test]
  fn is_send_sync() {
    crate::test_utils::assert_send::<super::AppHandle>();
    crate::test_utils::assert_sync::<super::AppHandle>();

    #[cfg(feature = "wry")]
    {
      crate::test_utils::assert_send::<super::AssetResolver<crate::Wry>>();
      crate::test_utils::assert_sync::<super::AssetResolver<crate::Wry>>();
    }
  }
}
