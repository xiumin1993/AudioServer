use eframe::egui;
use std::sync::mpsc;
use std::time::Instant;
use tokio::sync::mpsc as tokio_mpsc;

use audioserver::server::{run_server, ServerCommand, ServerConfig, ServerEvent, ServerStatus};

// ── v2 浅色主题配色 ──
mod colors {
    use eframe::egui::Color32;

    // 背景
    pub const BG_WHITE: Color32 = Color32::from_rgb(255, 255, 255);
    pub const BG_LIGHT: Color32 = Color32::from_rgb(250, 250, 250);
    // 边框
    pub const BORDER: Color32 = Color32::from_rgb(229, 229, 229);
    // 强调色（蓝）
    pub const ACCENT: Color32 = Color32::from_rgb(37, 99, 235);
    pub const ACCENT_BG: Color32 = Color32::from_rgb(240, 245, 255);
    pub const ACCENT_BORDER: Color32 = Color32::from_rgb(208, 223, 255);

    // 文字
    pub const TEXT_PRIMARY: Color32 = Color32::from_rgb(34, 34, 34);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(102, 102, 102);
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(153, 153, 153);
    pub const TEXT_DISABLED: Color32 = Color32::from_rgb(187, 187, 187);

    // 状态
    pub const GREEN: Color32 = Color32::from_rgb(34, 197, 94);
    pub const RED: Color32 = Color32::from_rgb(220, 38, 38);
    pub const RED_BG: Color32 = Color32::from_rgb(254, 242, 242);
    pub const RED_BORDER: Color32 = Color32::from_rgb(254, 202, 202);
    pub const WARN_TEXT: Color32 = Color32::from_rgb(180, 83, 9);
    pub const WARN_BG: Color32 = Color32::from_rgb(255, 251, 235);
    pub const WARN_BORDER: Color32 = Color32::from_rgb(253, 230, 138);

    // Toggle
    pub const TOGGLE_ON: Color32 = Color32::from_rgb(34, 197, 94);
    pub const TOGGLE_OFF: Color32 = Color32::from_rgb(221, 221, 221);
}

fn main() -> eframe::Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .format_timestamp_millis()
        .init();

    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_inner_size([420.0, 540.0])
            .with_resizable(true),
        ..Default::default()
    };

    eframe::run_native(
        "Audio Server",
        options,
        Box::new(|cc| {
            // 基于 egui 浅色主题，只覆盖需要的颜色
            let mut visuals = egui::Visuals::light();
            visuals.override_text_color = Some(colors::TEXT_PRIMARY);
            visuals.hyperlink_color = colors::ACCENT;
            visuals.faint_bg_color = colors::BG_LIGHT;
            visuals.extreme_bg_color = colors::BG_LIGHT;
            visuals.window_stroke = egui::Stroke::new(1.0_f32, colors::BORDER);
            visuals.widgets.noninteractive.bg_fill = colors::BG_WHITE;
            visuals.widgets.noninteractive.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_SECONDARY);
            visuals.widgets.inactive.bg_fill = colors::BG_WHITE;
            visuals.widgets.inactive.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_PRIMARY);
            visuals.widgets.hovered.bg_fill = colors::BG_LIGHT;
            visuals.widgets.hovered.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::TEXT_PRIMARY);
            visuals.widgets.active.bg_fill = colors::ACCENT_BG;
            visuals.widgets.active.fg_stroke =
                egui::Stroke::new(1.0_f32, colors::ACCENT);
            visuals.selection.bg_fill =
                egui::Color32::from_rgba_premultiplied(37, 99, 235, 30);
            visuals.selection.stroke = egui::Stroke::new(1.0_f32, colors::ACCENT);
            cc.egui_ctx.set_visuals(visuals);

            Ok(Box::new(AudioServerApp::new()))
        }),
    )
}

// ── Tab 枚举 ──
#[derive(Debug, Clone, Copy, PartialEq)]
enum AppTab {
    Connection,
    Settings,
    Log,
}

#[derive(Debug, Clone, Copy, PartialEq)]
#[allow(dead_code)]
enum ConnectionType {
    Wifi,
    Usb,
    Bluetooth,
}

struct AudioServerApp {
    port: String,
    sample_rate: String,
    channels: String,
    buffer_size: String,
    connection_type: ConnectionType,
    server_status: ServerStatus,
    server_handle: Option<std::thread::JoinHandle<()>>,
    cmd_tx: Option<tokio_mpsc::UnboundedSender<ServerCommand>>,
    event_rx: Option<mpsc::Receiver<ServerEvent>>,
    client_count: u64,
    logs: Vec<String>,
    #[allow(dead_code)]
    device_name: String,
    active_tab: AppTab,
    start_time: Option<Instant>,
}

impl AudioServerApp {
    fn new() -> Self {
        let mut app = Self {
            port: "8080".to_string(),
            sample_rate: "48000".to_string(),
            channels: "2".to_string(),
            buffer_size: "1024".to_string(),
            connection_type: ConnectionType::Wifi,
            server_status: ServerStatus::Stopped,
            server_handle: None,
            cmd_tx: None,
            event_rx: None,
            client_count: 0,
            logs: Vec::new(),
            device_name: String::new(),
            active_tab: AppTab::Connection,
            start_time: None,
        };
        // 启动时自动开启服务，不用手动点 Toggle
        app.start_server();
        app
    }

    fn poll_server_events(&mut self) {
        let mut events = Vec::new();
        if let Some(rx) = &self.event_rx {
            for _ in 0..50 {
                match rx.try_recv() {
                    Ok(event) => events.push(event),
                    Err(mpsc::TryRecvError::Empty) => break,
                    Err(mpsc::TryRecvError::Disconnected) => {
                        self.server_status = ServerStatus::Stopped;
                        self.cmd_tx = None;
                        self.event_rx = None;
                        self.start_time = None;
                        self.add_log("Server process ended".to_string());
                        return;
                    }
                }
            }
        }
        for event in events {
            self.handle_event(event);
        }
    }

    fn handle_event(&mut self, event: ServerEvent) {
        match event {
            ServerEvent::Log(msg) => self.add_log(msg),
            ServerEvent::StatusChanged(status) => {
                self.server_status = status;
                if status == ServerStatus::Running {
                    self.start_time = Some(Instant::now());
                } else {
                    self.start_time = None;
                }
            }
            ServerEvent::ClientConnected(_id) => self.client_count += 1,
            ServerEvent::ClientDisconnected(_id) => {
                self.client_count = self.client_count.saturating_sub(1);
            }
            ServerEvent::Error(msg) => {
                self.add_log(format!("[Error] {}", msg));
            }
        }
    }

    fn add_log(&mut self, msg: String) {
        let timestamp = chrono_now();
        self.logs.push(format!("[{}] {}", timestamp, msg));
        if self.logs.len() > 200 {
            self.logs.drain(0..50);
        }
    }

    fn start_server(&mut self) {
        if self.server_status == ServerStatus::Running {
            return;
        }
        let config = ServerConfig {
            port: self.port.parse().unwrap_or(8080),
            sample_rate: self.sample_rate.parse().unwrap_or(48000),
            channels: self.channels.parse().unwrap_or(2),
            buffer_size: self.buffer_size.parse().unwrap_or(1024),
        };
        let (event_tx, event_rx) = mpsc::channel();
        let (cmd_tx, cmd_rx) = tokio_mpsc::unbounded_channel();
        self.event_rx = Some(event_rx);
        self.cmd_tx = Some(cmd_tx);
        self.client_count = 0;
        self.logs.clear();
        self.start_time = Some(Instant::now());
        let handle = std::thread::spawn(move || {
            let rt = tokio::runtime::Runtime::new().unwrap();
            rt.block_on(run_server(config, event_tx, cmd_rx));
        });
        self.server_handle = Some(handle);
        self.add_log("Starting server...".to_string());
    }

    fn stop_server(&mut self) {
        if let Some(tx) = &self.cmd_tx {
            tx.send(ServerCommand::Stop).ok();
        }
        self.cmd_tx = None;
        self.event_rx = None;
        self.server_handle = None;
        self.server_status = ServerStatus::Stopped;
        self.start_time = None;
        self.add_log("Stop command sent".to_string());
    }

    fn get_local_ip() -> String {
        local_ip_address::local_ip()
            .map(|ip| ip.to_string())
            .unwrap_or_else(|_| "Unknown".to_string())
    }

    fn uptime_str(&self) -> String {
        match self.start_time {
            Some(start) => {
                let elapsed = start.elapsed().as_secs();
                let h = elapsed / 3600;
                let m = (elapsed % 3600) / 60;
                let s = elapsed % 60;
                format!("{:02}:{:02}:{:02}", h, m, s)
            }
            None => "00:00:00".to_string(),
        }
    }

    fn audio_summary(&self) -> String {
        format!("{}Hz · {}ch · PCM", self.sample_rate, self.channels)
    }

    // ── Header：状态 + 开关 ──
    fn show_header(&mut self, ui: &mut egui::Ui, is_running: bool) {
        ui.horizontal(|ui| {
            // 小圆点状态指示器
            let dot_size = 12.0_f32;
            let (dot_rect, _) = ui.allocate_exact_size(
                egui::vec2(dot_size, dot_size),
                egui::Sense::hover(),
            );
            if ui.is_rect_visible(dot_rect) {
                let dot_color = if is_running {
                    colors::GREEN
                } else {
                    colors::TEXT_DISABLED
                };
                ui.painter()
                    .circle_filled(dot_rect.center(), dot_size / 2.0, dot_color);
            }

            ui.add_space(10.0);

            // 状态文字
            ui.vertical(|ui| {
                let title = if is_running {
                    "Server Running"
                } else {
                    "Server Stopped"
                };
                ui.label(
                    egui::RichText::new(title)
                        .size(15.0)
                        .strong()
                        .color(colors::TEXT_PRIMARY),
                );
                let subtitle = if is_running {
                    format!("Uptime {}", self.uptime_str())
                } else {
                    "Toggle to start".to_string()
                };
                ui.label(
                    egui::RichText::new(subtitle)
                        .size(11.0)
                        .color(colors::TEXT_MUTED),
                );
            });

            // Toggle 开关
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let toggle_w = 44.0_f32;
                let toggle_h = 24.0_f32;
                let (toggle_rect, toggle_resp) = ui.allocate_exact_size(
                    egui::vec2(toggle_w, toggle_h),
                    egui::Sense::click(),
                );
                if toggle_resp.clicked() {
                    if is_running {
                        self.stop_server();
                    } else {
                        self.start_server();
                    }
                }
                if ui.is_rect_visible(toggle_rect) {
                    let knob_r = 10.0_f32;
                    let knob_y = toggle_rect.center().y;
                    let knob_x = if is_running {
                        toggle_rect.right() - 12.0_f32
                    } else {
                        toggle_rect.left() + 12.0_f32
                    };
                    let bg_color = if is_running {
                        colors::TOGGLE_ON
                    } else {
                        colors::TOGGLE_OFF
                    };
                    ui.painter().rect_filled(
                        toggle_rect,
                        egui::Rounding::same(toggle_h / 2.0),
                        bg_color,
                    );
                    // 白色旋钮，带一点阴影感
                    ui.painter().circle_filled(
                        egui::pos2(knob_x, knob_y),
                        knob_r,
                        egui::Color32::WHITE,
                    );
                    ui.painter().circle_stroke(
                        egui::pos2(knob_x, knob_y),
                        knob_r,
                        egui::Stroke::new(
                            0.5_f32,
                            egui::Color32::from_rgba_premultiplied(0, 0, 0, 25),
                        ),
                    );
                }
                if toggle_resp.hovered() {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            });
        });
    }

    // ── Tab 栏 ──
    fn show_tabs(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.set_height(38.0);
            let tabs = [
                (AppTab::Connection, "Connection"),
                (AppTab::Settings, "Settings"),
                (AppTab::Log, "Log"),
            ];
            let total_width = ui.available_width();
            let tab_width = total_width / tabs.len() as f32;

            for (tab, label) in &tabs {
                let is_active = self.active_tab == *tab;
                let (tab_rect, tab_resp) = ui.allocate_exact_size(
                    egui::vec2(tab_width, 38.0),
                    egui::Sense::click(),
                );
                if tab_resp.clicked() {
                    self.active_tab = *tab;
                }
                if ui.is_rect_visible(tab_rect) {
                    // 选中态：底部蓝色指示线
                    if is_active {
                        let line_h = 2.0_f32;
                        let line_rect = egui::Rect::from_min_size(
                            egui::pos2(tab_rect.left(), tab_rect.bottom() - line_h),
                            egui::vec2(tab_rect.width(), line_h),
                        );
                        ui.painter()
                            .rect_filled(line_rect, egui::Rounding::ZERO, colors::ACCENT);
                    }
                    let text_color = if is_active {
                        colors::ACCENT
                    } else {
                        colors::TEXT_MUTED
                    };
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.to_string(),
                            egui::FontId::proportional(13.0),
                            text_color,
                        )
                    });
                    let text_pos = egui::pos2(
                        tab_rect.center().x - galley.size().x / 2.0,
                        tab_rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(text_pos, galley, text_color);
                }
                if tab_resp.hovered() {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            }
        });
    }

    // ── Footer ──
    fn show_footer(&self, ui: &mut egui::Ui, is_running: bool) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(10.0)
                    .color(colors::TEXT_DISABLED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let summary = if is_running {
                    self.audio_summary()
                } else {
                    "Idle".to_string()
                };
                ui.label(
                    egui::RichText::new(summary)
                        .size(10.0)
                        .color(colors::TEXT_DISABLED),
                );
                ui.add_space(6.0);
                let dot_size = 6.0_f32;
                let (rect, _) = ui.allocate_exact_size(
                    egui::vec2(dot_size, dot_size),
                    egui::Sense::hover(),
                );
                if ui.is_rect_visible(rect) {
                    let dot_color = if is_running {
                        colors::GREEN
                    } else {
                        colors::TEXT_DISABLED
                    };
                    ui.painter()
                        .circle_filled(rect.center(), dot_size / 2.0, dot_color);
                }
            });
        });
    }

    // ── Connection Tab ──
    fn show_connection_tab(&mut self, ui: &mut egui::Ui) {
        let local_ip = Self::get_local_ip();

        // 连接方式按钮行
        ui.horizontal(|ui| {
            ui.set_height(56.0);
            let conns = [
                (ConnectionType::Wifi, "WiFi LAN", true),
                (ConnectionType::Usb, "USB", false),
                (ConnectionType::Bluetooth, "Bluetooth", false),
            ];
            let gap = 8.0_f32;
            let btn_w = (ui.available_width() - gap * 2.0) / 3.0;

            for (ct, label, enabled) in &conns {
                let is_active = self.connection_type == *ct && *enabled;
                let (resp_rect, resp) = ui.allocate_exact_size(
                    egui::vec2(btn_w, 52.0),
                    if *enabled {
                        egui::Sense::click()
                    } else {
                        egui::Sense::hover()
                    },
                );
                if ui.is_rect_visible(resp_rect) {
                    let (bg, border, text_color) = if is_active {
                        (colors::ACCENT_BG, colors::ACCENT, colors::ACCENT)
                    } else if *enabled {
                        (colors::BG_WHITE, colors::BORDER, colors::TEXT_SECONDARY)
                    } else {
                        (colors::BG_WHITE, colors::BORDER, colors::TEXT_DISABLED)
                    };
                    ui.painter().rect(
                        resp_rect,
                        egui::Rounding::same(8.0),
                        bg,
                        egui::Stroke::new(1.0_f32, border),
                    );
                    // 标签文字居中
                    let galley = ui.fonts(|f| {
                        f.layout_no_wrap(
                            label.to_string(),
                            egui::FontId::proportional(12.0),
                            text_color,
                        )
                    });
                    let text_pos = egui::pos2(
                        resp_rect.center().x - galley.size().x / 2.0,
                        resp_rect.center().y - galley.size().y / 2.0,
                    );
                    ui.painter().galley(text_pos, galley, text_color);
                }
                if resp.clicked() && *enabled {
                    self.connection_type = *ct;
                }
                if resp.hovered() && *enabled {
                    ui.output_mut(|o| o.cursor_icon = egui::CursorIcon::PointingHand);
                }
            }
        });

        ui.add_space(4.0);

        // 连接信息卡片
        let card_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        card_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("CONNECTION INFO")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);

            // IP 行
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("Local IP")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Copy").size(10.0).color(colors::ACCENT),
                            )
                            .fill(colors::ACCENT_BG)
                            .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                            .rounding(4.0),
                        )
                        .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = local_ip.clone());
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&local_ip)
                            .monospace()
                            .size(13.0)
                            .color(colors::ACCENT),
                    );
                });
            });

            ui.add_space(6.0);
            ui.separator();
            ui.add_space(6.0);

            // WebSocket 地址
            let ws_addr = format!("ws://{}:{}/ws/audio", local_ip, self.port);
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new("WebSocket")
                        .size(13.0)
                        .color(colors::TEXT_SECONDARY),
                );
                ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("Copy").size(10.0).color(colors::ACCENT),
                            )
                            .fill(colors::ACCENT_BG)
                            .stroke(egui::Stroke::new(1.0_f32, colors::ACCENT_BORDER))
                            .rounding(4.0),
                        )
                        .clicked()
                    {
                        ui.output_mut(|o| o.copied_text = ws_addr.clone());
                    }
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(&ws_addr)
                            .monospace()
                            .size(11.0)
                            .color(colors::ACCENT),
                    );
                });
            });
        });

        ui.add_space(4.0);

        // 客户端数量卡片
        let client_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        client_frame.show(ui, |ui| {
            ui.horizontal(|ui| {
                let count_color = if self.client_count > 0 {
                    colors::ACCENT
                } else {
                    colors::TEXT_DISABLED
                };
                ui.label(
                    egui::RichText::new(format!("{}", self.client_count))
                        .size(28.0)
                        .strong()
                        .color(count_color),
                );
                ui.add_space(8.0);
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new("Connected Clients")
                            .size(12.0)
                            .color(colors::TEXT_MUTED),
                    );
                    if self.client_count == 0 {
                        ui.label(
                            egui::RichText::new("Waiting...")
                                .size(11.0)
                                .color(colors::TEXT_DISABLED),
                        );
                    }
                });
            });
        });
    }

    // ── Settings Tab ──
    fn show_settings_tab(&mut self, ui: &mut egui::Ui, is_running: bool) {
        // 运行时锁定提示
        if is_running {
            let warn_frame = egui::Frame::none()
                .fill(colors::WARN_BG)
                .stroke(egui::Stroke::new(1.0_f32, colors::WARN_BORDER))
                .rounding(6.0)
                .inner_margin(egui::Margin::symmetric(12.0, 8.0));

            warn_frame.show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(egui::RichText::new("\u{26A0}").size(14.0).color(colors::WARN_TEXT));
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new("Stop the server to change settings.")
                            .size(12.0)
                            .color(colors::WARN_TEXT),
                    );
                });
            });
            ui.add_space(8.0);
        }

        // 音频参数卡片
        let audio_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        audio_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("AUDIO")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                egui::Grid::new("audio_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Sample Rate")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.sample_rate)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Channels")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.channels)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();

                        ui.label(
                            egui::RichText::new("Buffer Size")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.buffer_size)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });

        ui.add_space(8.0);

        // 网络参数卡片
        let net_frame = egui::Frame::none()
            .fill(colors::BG_WHITE)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(8.0)
            .inner_margin(egui::Margin::same(14.0));

        net_frame.show(ui, |ui| {
            ui.label(
                egui::RichText::new("NETWORK")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.add_space(8.0);
            ui.add_enabled_ui(!is_running, |ui| {
                egui::Grid::new("net_settings")
                    .num_columns(2)
                    .spacing([8.0, 6.0])
                    .show(ui, |ui| {
                        ui.label(
                            egui::RichText::new("Port")
                                .size(13.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.with_layout(
                            egui::Layout::right_to_left(egui::Align::Center),
                            |ui| {
                                ui.add(
                                    egui::TextEdit::singleline(&mut self.port)
                                        .desired_width(80.0)
                                        .horizontal_align(egui::Align::RIGHT),
                                );
                            },
                        );
                        ui.end_row();
                    });
            });
        });
    }

    // ── Log Tab ──
    fn show_log_tab(&mut self, ui: &mut egui::Ui) {
        // 工具栏
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new("RUNTIME LOG")
                    .size(11.0)
                    .color(colors::TEXT_MUTED),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                if ui
                    .add(
                        egui::Button::new(
                            egui::RichText::new("Clear").size(11.0).color(colors::RED),
                        )
                        .fill(colors::RED_BG)
                        .stroke(egui::Stroke::new(1.0_f32, colors::RED_BORDER))
                        .rounding(4.0),
                    )
                    .clicked()
                {
                    self.logs.clear();
                }
            });
        });

        ui.add_space(4.0);

        // 日志区域
        let log_frame = egui::Frame::none()
            .fill(colors::BG_LIGHT)
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .rounding(6.0)
            .inner_margin(egui::Margin::same(10.0));

        let available_height = ui.available_height() - 4.0;
        log_frame.show(ui, |ui| {
            egui::ScrollArea::vertical()
                .max_height(available_height)
                .stick_to_bottom(true)
                .show(ui, |ui| {
                    if self.logs.is_empty() {
                        ui.label(
                            egui::RichText::new("No logs yet")
                                .size(12.0)
                                .color(colors::TEXT_DISABLED),
                        );
                    }
                    for log in &self.logs {
                        let color = if log.contains("[Error]") {
                            colors::RED
                        } else if log.contains("started")
                            || log.contains("connected")
                            || log.contains("Device")
                        {
                            colors::GREEN
                        } else {
                            colors::TEXT_SECONDARY
                        };
                        ui.label(
                            egui::RichText::new(log)
                                .monospace()
                                .size(11.0)
                                .color(color),
                        );
                    }
                });
        });
    }
}

impl eframe::App for AudioServerApp {
    fn update(&mut self, ctx: &egui::Context, _frame: &mut eframe::Frame) {
        self.poll_server_events();
        let is_running = self.server_status == ServerStatus::Running;

        // 顶部面板：Header
        egui::TopBottomPanel::top("header_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::symmetric(16.0, 14.0)),
            )
            .show(ctx, |ui| {
                self.show_header(ui, is_running);
            });

        // 顶部面板：Tab 栏
        egui::TopBottomPanel::top("tab_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::symmetric(0.0, 0.0)),
            )
            .show(ctx, |ui| {
                self.show_tabs(ui);
            });

        // 底部面板：Footer
        egui::TopBottomPanel::bottom("footer_panel")
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_LIGHT)
                    .inner_margin(egui::Margin::symmetric(16.0, 6.0)),
            )
            .show(ctx, |ui| {
                self.show_footer(ui, is_running);
            });

        // 中央面板：Tab 内容
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(colors::BG_WHITE)
                    .inner_margin(egui::Margin::same(16.0)),
            )
            .show(ctx, |ui| {
                ui.spacing_mut().item_spacing = egui::vec2(8.0, 8.0);
                match self.active_tab {
                    AppTab::Connection => self.show_connection_tab(ui),
                    AppTab::Settings => self.show_settings_tab(ui, is_running),
                    AppTab::Log => self.show_log_tab(ui),
                }
            });

        ctx.request_repaint();
    }

    fn on_exit(&mut self, _gl: Option<&eframe::glow::Context>) {
        if self.server_status == ServerStatus::Running {
            self.stop_server();
        }
    }
}

fn chrono_now() -> String {
    use std::time::SystemTime;
    let now = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .unwrap_or_default();
    let secs = now.as_secs() % 86400;
    let hours = secs / 3600;
    let minutes = (secs % 3600) / 60;
    let seconds = secs % 60;
    format!("{:02}:{:02}:{:02}", hours, minutes, seconds)
}
