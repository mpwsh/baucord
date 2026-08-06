// ui/chat.rs
use crate::ui::messages::UiMessage;
use fltk::{prelude::*, *};
use i18n::t;
use ui::theme;
use ui::widgets::{controls, style};

pub struct Chat {
    // UI elements kept alive for method access.
    // sender, message_input, and char_count are captured by closures
    // in new() and don't need to live on the struct.
    group: group::Flex,
    chat_display: text::TextDisplay,
    chat_buffer: text::TextBuffer,
    style_buffer: text::TextBuffer,
}

impl Chat {
    pub fn new(sender: app::Sender<UiMessage>) -> Self {
        let mut chat_col = group::Flex::default().column();
        chat_col.set_margins(10, 10, 10, 10);
        chat_col.set_spacing(10);

        // Chat display with styled text
        let mut chat_display = text::TextDisplay::default();
        let chat_buffer = text::TextBuffer::default();
        let style_buffer = text::TextBuffer::default();

        chat_display.set_buffer(chat_buffer.clone());
        chat_display.wrap_mode(text::WrapMode::AtBounds, 0);
        style::style_text_display(&mut chat_display);

        // Style table: A=timestamp, B=username, C=message, D=system
        let style_table = vec![
            text::StyleTableEntryExt {
                color: theme::TEXT_MUTED,
                font: enums::Font::Helvetica,
                size: 12,
                attr: text::TextAttr::None,
                bgcolor: theme::SURFACE,
            },
            text::StyleTableEntryExt {
                color: theme::ACCENT,
                font: enums::Font::HelveticaBold,
                size: 13,
                attr: text::TextAttr::None,
                bgcolor: theme::SURFACE,
            },
            text::StyleTableEntryExt {
                color: theme::TEXT_SECONDARY,
                font: enums::Font::Helvetica,
                size: 13,
                attr: text::TextAttr::None,
                bgcolor: theme::SURFACE,
            },
            text::StyleTableEntryExt {
                color: theme::TEXT_MUTED,
                font: enums::Font::HelveticaItalic,
                size: 12,
                attr: text::TextAttr::None,
                bgcolor: theme::SURFACE,
            },
        ];

        chat_display.set_highlight_data_ext(style_buffer.clone(), style_table);

        // Input row
        let mut input_row = group::Flex::default().row();
        chat_col.fixed(&input_row, 40);
        input_row.set_spacing(10);

        let mut message_input = input::Input::default();
        message_input.set_maximum_size(400);
        style::style_input(&mut message_input);

        let mut char_count = frame::Frame::default().with_label("0/400");
        input_row.fixed(&char_count, 60);
        char_count.set_label_color(theme::TEXT_MUTED);
        char_count.set_label_size(11);

        let mut send_btn = controls::accent_button(&t!("send"), theme::ACCENT);
        input_row.fixed(&send_btn, 80);

        input_row.end();
        chat_col.end();

        // Update character counter + send on Enter
        let mut input_clone = message_input.clone();
        let mut count_clone = char_count.clone();

        message_input.handle(move |input, event| match event {
            enums::Event::KeyUp | enums::Event::KeyDown => {
                let len = input.value().len();
                count_clone.set_label(&format!("{len}/400"));

                if len > 380 {
                    count_clone.set_label_color(theme::WARNING);
                } else {
                    count_clone.set_label_color(theme::TEXT_MUTED);
                }

                if event == enums::Event::KeyDown && app::event_key() == enums::Key::Enter {
                    let text = input.value();
                    if !text.trim().is_empty() && text.len() <= 400 {
                        sender.send(UiMessage::SendChatMessage(text));
                        input.set_value("");
                        count_clone.set_label("0/400");
                        count_clone.set_label_color(theme::TEXT_MUTED);
                    }
                    return true;
                }

                false
            }
            _ => false,
        });

        // Send button
        send_btn.set_callback(move |_| {
            let text = input_clone.value();
            if !text.trim().is_empty() && text.len() <= 400 {
                sender.send(UiMessage::SendChatMessage(text));
                input_clone.set_value("");
                input_clone.do_callback();
            }
        });

        Self {
            group: chat_col,
            chat_display,
            chat_buffer,
            style_buffer,
        }
    }

    pub fn create_ui(&mut self) -> group::Flex {
        self.group.clone()
    }
    pub fn handle_system_message(&mut self, text: String) {
        let msg = format!("{text}\n");
        self.append_system_message(&msg);
        self.scroll_to_bottom();
    }
    pub fn handle_connected(&mut self, server_name: String, greeting: String) {
        self.chat_buffer.set_text("");
        self.style_buffer.set_text("");

        let msg = format!("{server_name}: {greeting}\n");
        self.append_system_message(&msg);
        self.scroll_to_bottom();
    }

    pub fn handle_disconnected(&mut self, reason: String) {
        let msg = format!("{}\n", t!("chat-disconnected", "reason" => reason.as_str()));
        self.append_system_message(&msg);
        self.scroll_to_bottom();
    }

    pub fn handle_user_joined(&mut self, username: String) {
        let msg = format!("{}\n", t!("chat-user-joined", "name" => username.as_str()));
        self.append_system_message(&msg);
        self.scroll_to_bottom();
    }

    pub fn handle_user_left(&mut self, username: String) {
        let msg = format!("{}\n", t!("chat-user-left", "name" => username.as_str()));
        self.append_system_message(&msg);
        self.scroll_to_bottom();
    }

    pub fn handle_text_message(&mut self, username: String, text: String) {
        let timestamp = chrono::Local::now().format("%H:%M");
        let timestamp_str = format!("[{timestamp}] ");
        let username_str = format!("{username}: ");
        let message_str = format!("{text}\n");

        self.chat_buffer.append(&timestamp_str);
        self.style_buffer.append(&"A".repeat(timestamp_str.len()));

        self.chat_buffer.append(&username_str);
        self.style_buffer.append(&"B".repeat(username_str.len()));

        self.chat_buffer.append(&message_str);
        self.style_buffer.append(&"C".repeat(message_str.len()));

        self.scroll_to_bottom();
    }

    fn append_system_message(&mut self, text: &str) {
        self.chat_buffer.append(text);
        self.style_buffer.append(&"D".repeat(text.len()));
    }

    fn scroll_to_bottom(&mut self) {
        let length = self.chat_buffer.length();
        self.chat_display.set_insert_position(length);
        self.chat_display
            .scroll(self.chat_display.count_lines(0, length, true), 0);
    }
}
