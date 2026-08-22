//! zeron-ui — the gpui viewport. Shell, sidebar, conversation, composer, terminal,
//! diff pane. Design-system primitives come from [`onyx_ui`].

pub use onyx_ui::gpui;
pub use onyx_ui::gpui_platform;

pub use onyx_ui::{
    appearance, badges, edge_fade, frost, icons, loaders, markdown, motion, popover,
    syntax_cache, theme,
};

pub mod app_menus;
pub mod attachments;
pub mod changes;
pub mod comments;
pub mod composer;
pub mod history;
pub mod notify;
pub mod pickers;
pub mod rail;
pub mod settings;
pub mod shell;
pub mod sound;
pub mod state;
pub mod terminal;
pub mod transcript;

use std::borrow::Cow;
use std::path::{Path, PathBuf};

use gpui::{App, AppContext as _, Bounds, TitlebarOptions, WindowBounds, WindowOptions, px, size};

static FONT_GEIST: &[u8] = include_bytes!("../assets/fonts/Geist.ttf");
static FONT_GEIST_MONO: &[u8] = include_bytes!("../assets/fonts/GeistMono.ttf");
static FONT_GEIST_MEDIUM: &[u8] = include_bytes!("../assets/fonts/Geist-Medium.ttf");
static FONT_GEIST_SEMIBOLD: &[u8] = include_bytes!("../assets/fonts/Geist-SemiBold.ttf");
static FONT_GEIST_BOLD: &[u8] = include_bytes!("../assets/fonts/Geist-Bold.ttf");

fn register_fonts(cx: &App) {
    if let Err(err) = cx.text_system().add_fonts(vec![
        Cow::Borrowed(FONT_GEIST),
        Cow::Borrowed(FONT_GEIST_MONO),
        Cow::Borrowed(FONT_GEIST_MEDIUM),
        Cow::Borrowed(FONT_GEIST_SEMIBOLD),
        Cow::Borrowed(FONT_GEIST_BOLD),
    ]) {
        tracing::warn!(error = %err, "failed to register embedded Geist fonts");
    }
}

fn wire_host_hooks() {
    appearance::set_persist_hook(Box::new(|mode, data_dir| {
        let mut settings = settings::UiSettings::load(data_dir);
        settings.appearance = mode;
        if let Err(err) = settings.save(data_dir) {
            tracing::warn!(error = %err, "could not persist appearance");
        }
    }));
    badges::register_extractor(comments::extract_badge);
}

pub use state::EngineBootConfig;
pub use zeron_proto::HarnessId;

#[derive(Debug, Clone)]
pub struct UiConfig {
    pub data_dir: PathBuf,
    pub ipc_port: u16,
    pub default_harness: HarnessId,
}

impl UiConfig {
    fn boot(&self) -> EngineBootConfig {
        EngineBootConfig {
            data_dir: self.data_dir.clone(),
            ipc_port: self.ipc_port,
            default_harness: self.default_harness,
        }
    }
}

struct ReopenState {
    state: gpui::Entity<state::AppState>,
    boot: EngineBootConfig,
}

impl gpui::Global for ReopenState {}

pub fn run_app(config: UiConfig) {
    wire_host_hooks();
    let app = gpui_platform::application().with_assets(icons::Assets);
    app.on_reopen(|cx| {
        if cx.windows().is_empty()
            && let Some(reopen) = cx.try_global::<ReopenState>()
        {
            let (state, boot) = (reopen.state.clone(), reopen.boot.clone());
            open_main_window(state, boot, cx);
        }
    });
    app.run(move |cx: &mut App| {
        gpui_tokio::init(cx);
        register_fonts(cx);
        let data_dir = config.boot().data_dir.clone();
        appearance::init(
            settings::UiSettings::load(&data_dir).appearance,
            data_dir,
            cx,
        );
        composer::init(cx);
        terminal::panel::init(cx);
        app_menus::init(cx);

        let state = cx.new(|_| state::AppState::new());
        state::AppState::bootstrap(state.clone(), config.boot(), cx);

        let quit_state = state.clone();
        cx.on_app_quit(move |cx| {
            let shutdown =
                quit_state.read(cx).engine().cloned().map(|handle| {
                    gpui_tokio::Tokio::spawn(cx, async move { handle.shutdown().await })
                });
            async move {
                if let Some(task) = shutdown {
                    let _ = task.await;
                }
            }
        })
        .detach();

        cx.set_global(ReopenState {
            state: state.clone(),
            boot: config.boot(),
        });
        open_main_window(state, config.boot(), cx);
        cx.set_menus(app_menus::app_menus());
        cx.activate(true);
    });
}

fn open_main_window(state: gpui::Entity<state::AppState>, boot: EngineBootConfig, cx: &mut App) {
    let bounds = Bounds::centered(None, size(px(1320.), px(880.)), cx);
    cx.open_window(
        WindowOptions {
            window_bounds: Some(WindowBounds::Windowed(bounds)),
            window_min_size: Some(size(px(900.), px(600.))),
            titlebar: Some(TitlebarOptions {
                title: None,
                appears_transparent: true,
                traffic_light_position: Some(gpui::point(px(14.), px(14.))),
            }),
            app_owns_titlebar_drag: true,
            window_decorations: cfg!(target_os = "linux")
                .then_some(gpui::WindowDecorations::Client),
            window_background: theme::Theme::of(cx).window_background_appearance(),
            app_id: Some("zeron".into()),
            ..Default::default()
        },
        move |window, cx| {
            appearance::observe_window(window, cx).detach();
            cx.new(|cx| shell::Shell::new(state, boot, cx))
        },
    )
    .expect("failed to open window");
    appearance::reapply_window_background(cx);
}
