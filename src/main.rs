use chrono::Local;
use dioxus::{
    desktop::{Config, LogicalPosition, WindowBuilder, use_window},
    prelude::*,
};
use log::{debug, error, info, warn};
use std::{
    env,
    process::Command,
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use shared_structures::{CommandType, SharedCommand, SharedMessage, SharedRingBuffer};
use xbar_core::initialize_logging;
use xbar_core::system_monitor::SystemMonitor;
use xbar_core::system_monitor::SystemSnapshot;
use xbar_core::{AudioManager, BrightnessManager};

const STYLE_CSS: &str = include_str!("../assets/style.css");

// ===== Nerd Font 图标（与 tauri_react_bar 对齐） =====
const TAG_ICONS: &[&str] = &[
    "\u{F0A1E}", // terminal
    "\u{F0239}", // browser
    "\u{F0A1B}", // code
    "\u{F0B79}", // chat
    "\u{F024B}", // folder
    "\u{F0388}", // music
    "\u{F0567}", // video
    "\u{F01F0}", // mail
    "\u{F0297}", // gamepad
];

const ICON_CPU: &str = "\u{F4BC}";
const ICON_MEM: &str = "\u{F035B}";
const ICON_BAT_FULL: &str = "\u{F0079}";
const ICON_BAT_CHG: &str = "\u{F0084}";
const ICON_VOL_HIGH: &str = "\u{F057E}";
const ICON_VOL_MID: &str = "\u{F0580}";
const ICON_VOL_LOW: &str = "\u{F057F}";
const ICON_VOL_MUTE: &str = "\u{F075F}";
const ICON_BRIGHT: &str = "\u{F00DE}";
const ICON_SHOT: &str = "\u{F0104}";
const ICON_TIME: &str = "\u{F0954}";
const ICON_MON: &str = "\u{F0379}";

fn monitor_icon_glyph(num: i32) -> &'static str {
    match num {
        0 => "\u{F02DA}",
        1 => "\u{F02DB}",
        _ => "?",
    }
}

fn volume_icon(snap: Option<&AudioSnapshot>) -> &'static str {
    match snap {
        None => ICON_VOL_MUTE,
        Some(s) if !s.has_device || s.is_muted || s.volume <= 0 => ICON_VOL_MUTE,
        Some(s) if s.volume < 34 => ICON_VOL_LOW,
        Some(s) if s.volume < 67 => ICON_VOL_MID,
        Some(_) => ICON_VOL_HIGH,
    }
}

#[derive(Clone, PartialEq, Debug)]
struct AudioSnapshot {
    volume: i32,
    is_muted: bool,
    has_device: bool,
}

#[derive(Clone, PartialEq, Debug)]
struct BrightnessSnapshot {
    percent: Option<u8>,
}

fn audio_snapshot_from(am: &AudioManager) -> AudioSnapshot {
    if let Some(d) = am.get_master_device() {
        AudioSnapshot {
            volume: d.volume.clamp(0, 100),
            is_muted: d.is_muted,
            has_device: true,
        }
    } else {
        AudioSnapshot {
            volume: 0,
            is_muted: true,
            has_device: false,
        }
    }
}

fn shared_memory_worker(
    shared_buffer_opt: Option<Arc<SharedRingBuffer>>,
    message_sender: tokio::sync::mpsc::Sender<SharedMessage>,
) {
    info!("Starting shared memory worker thread");
    let mut prev_timestamp = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    if let Some(shared_buffer) = shared_buffer_opt {
        loop {
            match shared_buffer.wait_for_message(Some(Duration::from_secs(2))) {
                Ok(true) => {
                    if let Ok(Some(message)) = shared_buffer.try_read_latest_message() {
                        if prev_timestamp != message.timestamp as u128 {
                            prev_timestamp = message.timestamp as u128;
                            if let Err(e) = message_sender.blocking_send(message) {
                                error!("Failed to send message: {}", e);
                                break;
                            }
                        }
                    }
                }
                Ok(false) => debug!("[notifier] Wait for message timed out."),
                Err(e) => {
                    error!("[notifier] Wait for message failed: {}", e);
                    break;
                }
            }
        }
    }
    info!("Shared memory worker task exiting");
}

fn send_tag_command(
    shared_buffer: &SharedRingBuffer,
    monitor_id: i32,
    active_tab: usize,
    is_view: bool,
) {
    let tag_bit = 1 << active_tab;
    the_cmd_send(
        shared_buffer,
        if is_view {
            SharedCommand::view_tag(tag_bit, monitor_id)
        } else {
            SharedCommand::toggle_tag(tag_bit, monitor_id)
        },
    );
}

fn the_cmd_send(shared_buffer: &SharedRingBuffer, cmd: SharedCommand) {
    match shared_buffer.send_command(cmd) {
        Ok(true) => info!("Sent command: by shared_buffer"),
        Ok(false) => warn!("Command buffer full, command dropped"),
        Err(e) => error!("Failed to send command: {}", e),
    }
}

fn send_layout_command(shared_buffer: &SharedRingBuffer, monitor_id: i32, layout_index: u32) {
    let cmd = SharedCommand::new(CommandType::SetLayout, layout_index, monitor_id);
    the_cmd_send(shared_buffer, cmd);
}

fn format_bytes(bytes: u64) -> String {
    const UNITS: &[&str] = &["B", "KB", "MB", "GB", "TB"];
    let mut size = bytes as f64;
    let mut unit_index = 0;
    while size >= 1024.0 && unit_index < UNITS.len() - 1 {
        size /= 1024.0;
        unit_index += 1;
    }
    if unit_index == 0 {
        format!("{:.0}{}", size, UNITS[unit_index])
    } else {
        format!("{:.1}{}", size, UNITS[unit_index])
    }
}

// 截图按钮
#[component]
fn ScreenshotButton() -> Element {
    let mut is_taking_screenshot = use_signal(|| false);

    let take_screenshot = move |_| {
        if is_taking_screenshot() {
            return;
        }
        is_taking_screenshot.set(true);
        info!("Taking screenshot with flameshot");
        spawn(async move {
            let result =
                tokio::task::spawn_blocking(|| Command::new("flameshot").arg("gui").spawn()).await;
            match result {
                Ok(Ok(mut child)) => {
                    info!("Flameshot launched successfully");
                    let _ = tokio::task::spawn_blocking(move || child.wait()).await;
                }
                Ok(Err(e)) => error!("Failed to launch flameshot: {}", e),
                Err(e) => error!("Task error when launching flameshot: {}", e),
            }
            is_taking_screenshot.set(false);
        });
    };

    let cls = if is_taking_screenshot() {
        "pill screenshot-pill taking"
    } else {
        "pill screenshot-pill"
    };

    rsx! {
        div {
            class: "{cls}",
            onclick: take_screenshot,
            title: "截图 (Flameshot)",
            span { class: "nf-icon", "{ICON_SHOT}" }
        }
    }
}

// 系统信息（CPU/MEM/电池）
#[component]
fn SystemInfoDisplay(snapshot: Option<SystemSnapshot>) -> Element {
    let sev = |p: f32| {
        if p <= 30.0 {
            "usage-good"
        } else if p <= 60.0 {
            "usage-warn"
        } else if p <= 80.0 {
            "usage-caution"
        } else {
            "usage-danger"
        }
    };

    if let Some(ref s) = snapshot {
        let cpu_class = sev(s.cpu_average as f32);
        let mem_class = sev(s.memory_usage_percent as f32);
        let batt_class = if s.battery_percent > 50.0 {
            "usage-good"
        } else if s.battery_percent > 20.0 {
            "usage-warn"
        } else {
            "usage-danger"
        };

        let cpu_cls = format!("pill usage-pill {}", cpu_class);
        let mem_cls = format!("pill usage-pill {}", mem_class);
        let batt_cls = format!("pill usage-pill {}", batt_class);

        let mem_title = format!(
            "内存使用: {} / {}",
            format_bytes(s.memory_used),
            format_bytes(s.memory_total)
        );
        let batt_title = if s.is_charging {
            format!("电池充电中: {:.1}%", s.battery_percent)
        } else {
            format!("电池电量: {:.1}%", s.battery_percent)
        };
        let batt_icon = if s.is_charging { ICON_BAT_CHG } else { ICON_BAT_FULL };
        let cpu_text = format!("{:.0}%", s.cpu_average);
        let mem_text = format!("{:.0}%", s.memory_usage_percent);
        let batt_text = format!("{:.0}%", s.battery_percent);

        rsx! {
            div { class: "system-info-container",
                div { class: "{cpu_cls}", title: "CPU 平均使用率",
                    span { class: "nf-icon", "{ICON_CPU}" }
                    "{cpu_text}"
                }
                div { class: "{mem_cls}", title: "{mem_title}",
                    span { class: "nf-icon", "{ICON_MEM}" }
                    "{mem_text}"
                }
                div { class: "{batt_cls}", title: "{batt_title}",
                    span { class: "nf-icon", "{batt_icon}" }
                    "{batt_text}"
                }
            }
        }
    } else {
        rsx! {
            div { class: "system-info-container",
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_CPU}" }
                    "--%"
                }
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_MEM}" }
                    "--%"
                }
                div { class: "pill usage-pill usage-warn",
                    span { class: "nf-icon", "{ICON_BAT_FULL}" }
                    "--%"
                }
            }
        }
    }
}

// 音量控制
#[component]
fn VolumeControl(
    snapshot: Option<AudioSnapshot>,
    audio_manager: Signal<Arc<Mutex<AudioManager>>>,
) -> Element {
    let snap = snapshot.clone();
    let muted = snap
        .as_ref()
        .map(|s| s.is_muted || !s.has_device)
        .unwrap_or(true);
    let has_device = snap.as_ref().map(|s| s.has_device).unwrap_or(false);
    let vol = snap.as_ref().map(|s| s.volume).unwrap_or(0);
    let icon = volume_icon(snap.as_ref());
    let label = if has_device {
        format!("{}%", vol)
    } else {
        "--".to_string()
    };
    let cls = if muted {
        "pill volume-pill muted"
    } else {
        "pill volume-pill"
    };

    let on_click = move |_| {
        let am = audio_manager.read().clone();
        if let Ok(mut am) = am.lock() {
            if let Some(dev) = am.get_master_device().cloned() {
                if let Err(e) = am.toggle_mute(&dev.name) {
                    warn!("toggle_mute failed: {}", e);
                }
            }
        }
    };
    let on_context = move |evt: Event<MouseData>| {
        evt.prevent_default();
        let am = audio_manager.read().clone();
        if let Ok(mut am) = am.lock() {
            if let Some(dev) = am.get_master_device().cloned() {
                let _ = am.adjust_volume(&dev.name, -5);
            }
        }
    };
    let on_wheel = move |evt: Event<WheelData>| {
        evt.prevent_default();
        let dy = evt.data().delta().strip_units().y;
        let delta = if dy < 0.0 { 5 } else { -5 };
        let am = audio_manager.read().clone();
        if let Ok(mut am) = am.lock() {
            if let Some(dev) = am.get_master_device().cloned() {
                let _ = am.adjust_volume(&dev.name, delta);
            }
        }
    };

    rsx! {
        div {
            class: "{cls}",
            onclick: on_click,
            oncontextmenu: on_context,
            onwheel: on_wheel,
            title: "左键静音 / 滚轮调节 / 右键 -5",
            span { class: "nf-icon", "{icon}" }
            "{label}"
        }
    }
}

// 亮度控制
#[component]
fn BrightnessControl(
    snapshot: Option<BrightnessSnapshot>,
    brightness_manager: Signal<Arc<Mutex<BrightnessManager>>>,
) -> Element {
    let pct = snapshot.as_ref().and_then(|s| s.percent);
    let label = match pct {
        Some(p) => format!("{}%", p),
        None => "--".to_string(),
    };

    let on_click = move |_| {
        let bm = brightness_manager.read().clone();
        if let Ok(mut bm) = bm.lock() {
            bm.adjust(5);
        }
    };
    let on_context = move |evt: Event<MouseData>| {
        evt.prevent_default();
        let bm = brightness_manager.read().clone();
        if let Ok(mut bm) = bm.lock() {
            bm.adjust(-5);
        }
    };
    let on_wheel = move |evt: Event<WheelData>| {
        evt.prevent_default();
        let dy = evt.data().delta().strip_units().y;
        let delta = if dy < 0.0 { 5 } else { -5 };
        let bm = brightness_manager.read().clone();
        if let Ok(mut bm) = bm.lock() {
            bm.adjust(delta);
        }
    };

    rsx! {
        div {
            class: "pill brightness-pill",
            onclick: on_click,
            oncontextmenu: on_context,
            onwheel: on_wheel,
            title: "左键加亮 / 右键减暗 / 滚轮调节",
            span { class: "nf-icon", "{ICON_BRIGHT}" }
            "{label}"
        }
    }
}

// 时间文本
#[component]
fn TimeText(show_seconds: bool) -> Element {
    let mut current_time = use_signal(|| Local::now());

    use_effect(move || {
        spawn(async move {
            loop {
                let update_interval = if show_seconds {
                    Duration::from_millis(1000)
                } else {
                    Duration::from_millis(60000)
                };
                tokio::time::sleep(update_interval).await;
                current_time.set(Local::now());
            }
        });
    });

    let time_format = if show_seconds {
        "%Y-%m-%d %H:%M:%S"
    } else {
        "%Y-%m-%d %H:%M"
    };
    let time_str = current_time().format(time_format).to_string();

    rsx! {
        span { "{time_str}" }
    }
}

#[derive(Debug, Clone, PartialEq)]
enum ButtonState {
    Filtered,
    Selected,
    Urgent,
    Occupied,
    Default,
}

impl ButtonState {
    fn from_flags(is_filtered: bool, is_selected: bool, is_urg: bool, is_occ: bool) -> Self {
        if is_filtered {
            ButtonState::Filtered
        } else if is_selected {
            ButtonState::Selected
        } else if is_urg {
            ButtonState::Urgent
        } else if is_occ {
            ButtonState::Occupied
        } else {
            ButtonState::Default
        }
    }

    fn to_css_class(&self) -> &'static str {
        match self {
            ButtonState::Filtered => "emoji-button state-filtered",
            ButtonState::Selected => "emoji-button state-selected",
            ButtonState::Urgent => "emoji-button state-urgent",
            ButtonState::Occupied => "emoji-button state-occupied",
            ButtonState::Default => "emoji-button state-default",
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq)]
struct ButtonStateData {
    is_filtered: bool,
    is_selected: bool,
    is_urg: bool,
    is_occ: bool,
}

impl ButtonStateData {
    fn get_state(&self) -> ButtonState {
        ButtonState::from_flags(self.is_filtered, self.is_selected, self.is_urg, self.is_occ)
    }
}

fn get_button_class(index: usize, button_states: &[ButtonStateData]) -> &'static str {
    if index < button_states.len() {
        button_states[index].get_state().to_css_class()
    } else {
        "emoji-button state-default"
    }
}

fn main() {
    let args: Vec<String> = env::args().collect();
    let shared_path = args.get(1).cloned().unwrap_or_default();
    if let Err(e) = initialize_logging("dioxus_bar", &shared_path) {
        error!("Failed to initialize logging: {}", e);
        std::process::exit(1);
    }
    info!("=== Environment Debug Info ===");
    info!("DISPLAY: {:?}", env::var("DISPLAY"));
    info!("WAYLAND_DISPLAY: {:?}", env::var("WAYLAND_DISPLAY"));
    info!("XDG_SESSION_TYPE: {:?}", env::var("XDG_SESSION_TYPE"));
    info!("DESKTOP_SESSION: {:?}", env::var("DESKTOP_SESSION"));
    info!("XDG_CURRENT_DESKTOP: {:?}", env::var("XDG_CURRENT_DESKTOP"));
    if let Ok(output) = Command::new("xrandr").arg("--current").output() {
        let output_str = String::from_utf8_lossy(&output.stdout);
        for line in output_str.lines() {
            if line.contains("primary") || line.contains("*") {
                info!("Screen info: {}", line.trim());
            }
        }
    }
    info!("Starting dioxus_bar v{}", 1.0);

    dioxus::LaunchBuilder::desktop()
        .with_cfg(
            Config::new().with_window(
                WindowBuilder::new()
                    .with_title("dioxus_bar")
                    .with_position(LogicalPosition::new(0, 0))
                    .with_maximizable(false)
                    .with_minimizable(false)
                    .with_resizable(true)
                    .with_always_on_top(true)
                    .with_visible_on_all_workspaces(true)
                    .with_decorations(false),
            ),
        )
        .launch(App);
}

#[component]
fn App() -> Element {
    let shared_path = std::env::args().nth(1).unwrap_or_else(|| {
        std::env::var("SHARED_MEMORY_PATH").unwrap_or_else(|_| "/dev/shm/monitor_0".to_string())
    });

    let window = use_window();
    let scale_factor = use_signal(|| window.scale_factor());

    let mut button_states = use_signal(|| vec![ButtonStateData::default(); TAG_ICONS.len()]);
    let mut last_update = use_signal(|| Instant::now());
    let mut show_seconds = use_signal(|| true);
    let mut system_snapshot = use_signal(|| None::<SystemSnapshot>);
    let mut audio_snapshot = use_signal(|| None::<AudioSnapshot>);
    let mut brightness_snapshot = use_signal(|| None::<BrightnessSnapshot>);
    let mut pressed_button = use_signal(|| None::<usize>);
    let mut monitor_num = use_signal(|| None::<i32>);
    let mut layout_symbol = use_signal(|| "[]=".to_string());
    let mut layout_open = use_signal(|| false);

    let shared_buffer_sig = use_signal(|| {
        info!(
            "(SIGNAL-INIT) Creating shared ring buffer for path: {}",
            shared_path
        );
        SharedRingBuffer::create_shared_ring_buffer_aux(&shared_path).map(Arc::new)
    });

    let audio_manager = use_signal(|| Arc::new(Mutex::new(AudioManager::new())));
    let brightness_manager = use_signal(|| Arc::new(Mutex::new(BrightnessManager::new())));

    // 系统信息
    use_effect(move || {
        spawn(async move {
            let (sys_sender, sys_receiver) = std::sync::mpsc::channel();
            thread::spawn(move || {
                let mut monitor = SystemMonitor::new(30);
                monitor.set_update_interval(Duration::from_millis(2000));
                loop {
                    monitor.update_if_needed();
                    if let Some(snapshot) = monitor.get_snapshot() {
                        if sys_sender.send(snapshot.clone()).is_err() {
                            break;
                        }
                    }
                    thread::sleep(Duration::from_millis(500));
                }
            });
            loop {
                if let Ok(snapshot) = sys_receiver.try_recv() {
                    system_snapshot.set(Some(snapshot));
                }
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
        });
    });

    // 音频 + 亮度刷新
    use_effect(move || {
        let am = audio_manager.read().clone();
        let bm = brightness_manager.read().clone();
        spawn(async move {
            loop {
                let new_audio = {
                    let mut guard = am.lock().ok();
                    if let Some(g) = guard.as_mut() {
                        g.update_if_needed();
                        Some(audio_snapshot_from(g))
                    } else {
                        None
                    }
                };
                if let Some(snap) = new_audio {
                    let changed = audio_snapshot.read().as_ref() != Some(&snap);
                    if changed {
                        audio_snapshot.set(Some(snap));
                    }
                }

                let new_brightness = {
                    let mut guard = bm.lock().ok();
                    if let Some(g) = guard.as_mut() {
                        g.update_if_needed();
                        Some(BrightnessSnapshot { percent: g.percent() })
                    } else {
                        None
                    }
                };
                if let Some(snap) = new_brightness {
                    let changed = brightness_snapshot.read().as_ref() != Some(&snap);
                    if changed {
                        brightness_snapshot.set(Some(snap));
                    }
                }

                tokio::time::sleep(Duration::from_millis(750)).await;
            }
        });
    });

    // 共享内存
    use_effect(move || {
        let (message_sender, mut message_receiver) =
            tokio::sync::mpsc::channel::<SharedMessage>(100);
        info!("Using shared memory path: {}", shared_path);
        let shared_buffer_for_worker = shared_buffer_sig.read().clone();
        thread::spawn(move || {
            shared_memory_worker(shared_buffer_for_worker, message_sender);
        });

        spawn(async move {
            while let Some(shared_message) = message_receiver.recv().await {
                let mut new_states = vec![ButtonStateData::default(); TAG_ICONS.len()];
                let monitor_info = shared_message.monitor_info;

                layout_symbol.set(monitor_info.get_ltsymbol());
                monitor_num.set(Some(monitor_info.monitor_num));

                for (index, tag_status) in monitor_info.tag_status_vec.iter().enumerate() {
                    if index < new_states.len() {
                        new_states[index] = ButtonStateData {
                            is_filtered: tag_status.is_filled,
                            is_selected: tag_status.is_selected,
                            is_urg: tag_status.is_urg,
                            is_occ: tag_status.is_occ,
                        };
                    }
                }
                let need_update = { *button_states.read() != new_states };
                if need_update {
                    button_states.set(new_states);
                    last_update.set(Instant::now());
                }
            }
        });
    });

    let mut handle_button_press = move |index: usize| {
        info!("Button {} pressed", index);
        pressed_button.set(Some(index));
    };

    let mut handle_button_release = move |index: usize| {
        info!("Button {} released", index);
        pressed_button.set(None);
        if let (Some(monitor_id), Some(buffer_arc)) =
            (monitor_num(), shared_buffer_sig.read().as_ref())
        {
            send_tag_command(buffer_arc, monitor_id, index, true);
        } else {
            warn!("Shared buffer or monitor_num not available, cannot send command.");
        }
    };

    let mut handle_button_leave = move |_index: usize| {
        pressed_button.set(None);
    };

    let toggle_layout_panel = move |_| {
        layout_open.set(!layout_open());
    };

    let mut select_layout = move |idx: u32| {
        layout_open.set(false);
        if let (Some(monitor_id), Some(buffer_arc)) =
            (monitor_num(), shared_buffer_sig.read().as_ref())
        {
            send_layout_command(buffer_arc, monitor_id, idx);
        } else {
            warn!("Shared buffer or monitor_num not available for layout set.");
        }
    };

    rsx! {
        document::Style { "{STYLE_CSS}" }

        div { class: "button-row",

            div { class: "buttons-container",
                for (i , icon) in TAG_ICONS.iter().enumerate() {
                    {
                        let base_class = get_button_class(i, &button_states());
                        let is_pressed = pressed_button() == Some(i);
                        let button_class = if is_pressed {
                            format!("{} pressed", base_class)
                        } else {
                            base_class.to_string()
                        };
                        rsx! {
                            button {
                                key: "{i}",
                                class: "{button_class}",
                                title: "Tag {i + 1}",
                                onmousedown: move |_| handle_button_press(i),
                                onmouseup: move |_| handle_button_release(i),
                                onmouseleave: move |_| handle_button_leave(i),
                                onclick: move |_| {
                                    if pressed_button() == Some(i) {
                                        handle_button_release(i);
                                    }
                                },
                                span { class: "nf-icon", "{icon}" }
                            }
                        }
                    }
                }

                // 布局切换
                div { class: "layout-controls",
                    {
                        let toggle_class = format!(
                            "pill layout-toggle {}",
                            if layout_open() { "open" } else { "closed" },
                        );
                        rsx! {
                            div { class: "{toggle_class}", onclick: toggle_layout_panel, "{layout_symbol()}" }
                        }
                    }

                    if layout_open() {
                        {
                            let current = layout_symbol();
                            let lo0 = format!(
                                "pill layout-option {}",
                                if current.contains("[]=") { "current" } else { "" },
                            );
                            let lo1 = format!(
                                "pill layout-option {}",
                                if current.contains("><>") { "current" } else { "" },
                            );
                            let lo2 = format!(
                                "pill layout-option {}",
                                if current.contains("[M]") { "current" } else { "" },
                            );
                            rsx! {
                                div { class: "layout-selector",
                                    div { class: "{lo0}", onclick: move |_| select_layout(0), "[]=" }
                                    div { class: "{lo1}", onclick: move |_| select_layout(1), "><>" }
                                    div { class: "{lo2}", onclick: move |_| select_layout(2), "[M]" }
                                }
                            }
                        }
                    }
                }
            }

            div { class: "spacer" }

            div { class: "right-info-container",
                SystemInfoDisplay { snapshot: system_snapshot() }
                BrightnessControl {
                    snapshot: brightness_snapshot(),
                    brightness_manager,
                }
                VolumeControl {
                    snapshot: audio_snapshot(),
                    audio_manager,
                }
                ScreenshotButton {}

                div {
                    class: "pill time-pill",
                    onclick: move |_| {
                        show_seconds.set(!show_seconds());
                        info!("Toggle seconds display: {}", show_seconds());
                    },
                    span { class: "nf-icon", "{ICON_TIME}" }
                    TimeText { show_seconds: show_seconds() }
                }

                div { class: "pill monitor-pill",
                    span { class: "nf-icon", "{ICON_MON}" }
                    {format!("{}", monitor_icon_glyph(monitor_num().unwrap_or(0)))}
                }

                div { class: "pill scale-pill", {format!("s: {:.2}", scale_factor())} }
            }
        }
    }
}
