use chrono::{DateTime, Utc};
use egui::{Color32, RichText};
use std::collections::VecDeque;

#[derive(Debug, Clone, Copy)]
pub enum NotificationType {
    Info,
    Success,
    // Warning,
    Error,
}

#[derive(Debug, Clone)]
pub struct Notification {
    pub id: usize,
    pub message: String,
    pub notification_type: NotificationType,
    pub created_at: DateTime<Utc>,
    pub duration_secs: f32,
}

impl Notification {
    pub fn new(message: String, notification_type: NotificationType) -> Self {
        Self {
            id: 0, // Will be set by the manager
            message,
            notification_type,
            created_at: Utc::now(),
            duration_secs: match notification_type {
                NotificationType::Error => 8.0,
                // NotificationType::Warning => 6.0,
                NotificationType::Success => 4.0,
                NotificationType::Info => 3.0,
            },
        }
    }

    /*
    pub fn with_duration(mut self, duration_secs: f32) -> Self {
        self.duration_secs = duration_secs;
        self
    } */

    pub fn is_expired(&self) -> bool {
        let elapsed = Utc::now().signed_duration_since(self.created_at);
        elapsed.num_milliseconds() as f32 / 1000.0 > self.duration_secs
    }

    pub fn get_color(&self) -> Color32 {
        match self.notification_type {
            NotificationType::Info => Color32::LIGHT_BLUE,
            NotificationType::Success => Color32::LIGHT_GREEN,
            // NotificationType::Warning => Color32::from_rgb(255, 165, 0), // Orange
            NotificationType::Error => Color32::LIGHT_RED,
        }
    }

    pub fn get_icon(&self) -> &'static str {
        match self.notification_type {
            NotificationType::Info => "ℹ️",
            NotificationType::Success => "✅",
            // NotificationType::Warning => "⚠️",
            NotificationType::Error => "❌",
        }
    }
}

pub struct NotificationManager {
    notifications: VecDeque<Notification>,
    next_id: usize,
    max_notifications: usize,
}

impl Default for NotificationManager {
    fn default() -> Self {
        Self {
            notifications: VecDeque::new(),
            next_id: 1,
            max_notifications: 5,
        }
    }
}

impl NotificationManager {
    pub fn add_notification(&mut self, mut notification: Notification) {
        notification.id = self.next_id;
        self.next_id += 1;

        // Remove oldest if we exceed max
        if self.notifications.len() >= self.max_notifications {
            self.notifications.pop_front();
        }

        self.notifications.push_back(notification);
    }

    pub fn add_info(&mut self, message: impl Into<String>) {
        let notification = Notification::new(message.into(), NotificationType::Info);
        self.add_notification(notification);
    }

    pub fn add_success(&mut self, message: impl Into<String>) {
        let notification = Notification::new(message.into(), NotificationType::Success);
        self.add_notification(notification);
    }

    /*
    pub fn add_warning(&mut self, message: impl Into<String>) {
        let notification = Notification::new(message.into(), NotificationType::Warning);
        self.add_notification(notification);
    } */

    pub fn add_error(&mut self, message: impl Into<String>) {
        let notification = Notification::new(message.into(), NotificationType::Error);
        self.add_notification(notification);
    }

    pub fn remove_notification(&mut self, id: usize) {
        self.notifications.retain(|n| n.id != id);
    }

    pub fn update(&mut self) {
        // Remove expired notifications
        self.notifications.retain(|n| !n.is_expired());
    }

    pub fn ui(&mut self, ctx: &egui::Context) {
        let mut to_remove = Vec::new();

        // Show notifications in top-right corner
        egui::Area::new("notifications".into())
            .anchor(egui::Align2::RIGHT_TOP, egui::vec2(-20.0, 20.0))
            .show(ctx, |ui| {
                ui.set_max_width(350.0);

                for notification in &self.notifications {
                    let response = self.render_notification(ui, notification);
                    if response.clicked() {
                        to_remove.push(notification.id);
                    }
                }
            });

        // Remove clicked notifications
        for id in to_remove {
            self.remove_notification(id);
        }
    }

    fn render_notification(
        &self,
        ui: &mut egui::Ui,
        notification: &Notification,
    ) -> egui::Response {
        let color = notification.get_color();
        let icon = notification.get_icon();

        let frame = egui::Frame::default()
            .fill(color.gamma_multiply(0.1))
            .stroke(egui::Stroke::new(1.0, color))
            .rounding(egui::Rounding::same(8.0))
            .inner_margin(egui::Margin::same(12.0))
            .shadow(egui::epaint::Shadow {
                offset: egui::vec2(2.0, 4.0),
                blur: 8.0,
                spread: 0.0,
                color: Color32::from_black_alpha(50),
            });

        frame
            .show(ui, |ui| {
                ui.horizontal(|ui| {
                    ui.label(RichText::new(icon).size(16.0));
                    ui.vertical(|ui| {
                        ui.label(RichText::new(&notification.message).color(color).strong());

                        // Show time remaining as a progress bar
                        let elapsed = Utc::now()
                            .signed_duration_since(notification.created_at)
                            .num_milliseconds() as f32
                            / 1000.0;
                        let progress = 1.0 - (elapsed / notification.duration_secs).clamp(0.0, 1.0);

                        let progress_bar = egui::ProgressBar::new(progress)
                            .desired_width(250.0)
                            .desired_height(3.0)
                            .fill(color.gamma_multiply(0.8));

                        ui.add(progress_bar);
                    });

                    ui.with_layout(egui::Layout::right_to_left(egui::Align::TOP), |ui| {
                        if ui.small_button("✖").on_hover_text("Dismiss").clicked() {
                            // Will be handled by the caller
                        }
                    });
                });
            })
            .response
    }
}
