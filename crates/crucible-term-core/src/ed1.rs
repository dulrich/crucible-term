//! The ED 1 correction: a `Handler` wrapper around rio-vt's `Crosswords` that delegates every
//! operation verbatim and intercepts only `clear_screen(ClearMode::Above)`.
//!
//! rio-vt 0.5.29 inherits alacritty's bug where `CSI 1 J` clears the rows above the cursor only
//! `if cursor.row > 1` (`rio-vt-0.5.29/src/crosswords/mod.rs:4129`), so with the cursor on the
//! second row the first row survives. After delegating, the wrapper clears row 0 itself when the
//! cursor sits on row 1 — exactly what the upstream branch does for every other row (the same
//! `reset_region` with the same cursor template; row 0 is already marked damaged by rio-vt).

use cursor_icon::CursorIcon;
use rio_graphics::GraphicData;
use rio_vt::ansi::glyph_protocol;
use rio_vt::ansi::kitty_graphics_protocol;
use rio_vt::ansi::mode::{Mode, PrivateMode};
use rio_vt::ansi::{
    ClearMode, CursorShape, KeyboardModes, KeyboardModesApplyBehavior, LineClearMode,
    TabulationClearMode, sixel,
};
use rio_vt::config::colors::ColorRgb;
use rio_vt::crosswords::Crosswords;
use rio_vt::crosswords::attr::Attr;
use rio_vt::crosswords::grid::row::SemanticPrompt;
use rio_vt::crosswords::pos::{CharsetIndex, Column, Line, StandardCharset};
use rio_vt::crosswords::square::Hyperlink;
use rio_vt::event::{EventListener, ProgressReport};
use rio_vt::performer::handler::{Handler, ScpCharPath, ScpUpdateMode};
use rio_vt::performer::parser::Params;
use std::path::PathBuf;

/// Delegating wrapper; see the module docs.
pub(crate) struct Ed1Fix<'a, U: EventListener>(pub(crate) &'a mut Crosswords<U>);

macro_rules! delegate {
    ($( fn $name:ident(&mut self $(, $arg:ident : $ty:ty)*) $(-> $ret:ty)?; )*) => {
        $(
            #[inline]
            fn $name(&mut self $(, $arg: $ty)*) $(-> $ret)? {
                <Crosswords<U> as Handler>::$name(self.0 $(, $arg)*)
            }
        )*
    };
}

impl<U: EventListener> Handler for Ed1Fix<'_, U> {
    fn clear_screen(&mut self, mode: ClearMode) {
        let above = matches!(mode, ClearMode::Above);
        <Crosswords<U> as Handler>::clear_screen(self.0, mode);
        if above && self.0.grid.cursor.pos.row == Line(1) {
            self.0.grid.reset_region(..Line(1));
        }
    }

    fn is_sixel_graphic_active(&self) -> bool {
        <Crosswords<U> as Handler>::is_sixel_graphic_active(self.0)
    }

    fn kitty_chunking_state_mut(
        &mut self,
    ) -> Option<&mut kitty_graphics_protocol::KittyGraphicsState> {
        <Crosswords<U> as Handler>::kitty_chunking_state_mut(self.0)
    }

    delegate! {
        fn set_title(&mut self, a: Option<String>);
        fn set_current_directory(&mut self, a: PathBuf);
        fn set_semantic_prompt(&mut self, a: SemanticPrompt);
        fn set_user_var(&mut self, name: String, value: String);
        fn set_cursor_style(&mut self, style: Option<CursorShape>, blinking: bool);
        fn set_cursor_shape(&mut self, shape: CursorShape);
        fn input(&mut self, c: char);
        fn input_str(&mut self, s: &str);
        fn input_ascii_str(&mut self, s: &str);
        fn input_codepoints(&mut self, cps: &[u32]);
        fn goto(&mut self, l: Line, c: Column);
        fn goto_line(&mut self, l: Line);
        fn goto_col(&mut self, c: Column);
        fn insert_blank(&mut self, n: usize);
        fn move_up(&mut self, n: usize);
        fn move_down(&mut self, n: usize);
        fn identify_terminal(&mut self, i: Option<char>);
        fn device_status(&mut self, n: usize);
        fn move_forward(&mut self, c: Column);
        fn move_backward(&mut self, c: Column);
        fn move_down_and_cr(&mut self, n: usize);
        fn move_up_and_cr(&mut self, n: usize);
        fn put_tab(&mut self, n: u16);
        fn backspace(&mut self);
        fn carriage_return(&mut self);
        fn linefeed(&mut self);
        fn bell(&mut self);
        fn desktop_notification(&mut self, title: String, body: String);
        fn substitute(&mut self);
        fn newline(&mut self);
        fn set_horizontal_tabstop(&mut self);
        fn scroll_up(&mut self, n: usize);
        fn scroll_down(&mut self, n: usize);
        fn insert_blank_lines(&mut self, n: usize);
        fn delete_lines(&mut self, n: usize);
        fn erase_chars(&mut self, c: Column);
        fn delete_chars(&mut self, n: usize);
        fn move_backward_tabs(&mut self, n: u16);
        fn move_forward_tabs(&mut self, n: u16);
        fn save_cursor_position(&mut self);
        fn restore_cursor_position(&mut self);
        fn clear_line(&mut self, m: LineClearMode);
        fn set_tabs(&mut self, n: u16);
        fn clear_tabs(&mut self, m: TabulationClearMode);
        fn reset_state(&mut self);
        fn reverse_index(&mut self);
        fn terminal_attribute(&mut self, a: Attr);
        fn set_mode(&mut self, m: Mode);
        fn unset_mode(&mut self, m: Mode);
        fn report_mode(&mut self, m: Mode);
        fn set_private_mode(&mut self, m: PrivateMode);
        fn unset_private_mode(&mut self, m: PrivateMode);
        fn report_private_mode(&mut self, m: PrivateMode);
        fn report_version(&mut self);
        fn set_scrolling_region(&mut self, top: usize, bottom: Option<usize>);
        fn set_keypad_application_mode(&mut self);
        fn unset_keypad_application_mode(&mut self);
        fn set_active_charset(&mut self, c: CharsetIndex);
        fn configure_charset(&mut self, i: CharsetIndex, c: StandardCharset);
        fn set_color(&mut self, i: usize, c: ColorRgb);
        fn dynamic_color_sequence(&mut self, p: String, i: usize, t: &str);
        fn reset_color(&mut self, i: usize);
        fn clipboard_store(&mut self, c: u8, d: &[u8]);
        fn clipboard_load(&mut self, c: u8, t: &str);
        fn decaln(&mut self);
        fn push_title(&mut self);
        fn pop_title(&mut self);
        fn text_area_size_pixels(&mut self);
        fn cells_size_pixels(&mut self);
        fn text_area_size_chars(&mut self);
        fn graphics_attribute(&mut self, a: u16, b: u16);
        fn sixel_graphic_start(&mut self, p: &Params);
        fn sixel_graphic_put(&mut self, b: u8) -> Result<(), sixel::Error>;
        fn sixel_graphic_reset(&mut self);
        fn sixel_graphic_finish(&mut self);
        fn insert_graphic(&mut self, d: GraphicData, p: Option<Vec<ColorRgb>>, m: Option<u8>);
        fn store_graphic(&mut self, d: GraphicData);
        fn kitty_transmit_and_display(&mut self, d: GraphicData, p: kitty_graphics_protocol::PlacementRequest);
        fn place_graphic(&mut self, p: kitty_graphics_protocol::PlacementRequest) -> bool;
        fn delete_graphics(&mut self, d: kitty_graphics_protocol::DeleteRequest);
        fn set_hyperlink(&mut self, h: Option<Hyperlink>);
        fn set_mouse_cursor_icon(&mut self, i: CursorIcon);
        fn set_progress_report(&mut self, r: ProgressReport);
        fn report_keyboard_mode(&mut self);
        fn push_keyboard_mode(&mut self, m: KeyboardModes);
        fn pop_keyboard_modes(&mut self, n: u16);
        fn set_keyboard_mode(&mut self, m: KeyboardModes, b: KeyboardModesApplyBehavior);
        fn xtgettcap_response(&mut self, r: String);
        fn set_modify_other_keys(&mut self, l: u8);
        fn kitty_graphics_response(&mut self, r: String);
        fn glyph_protocol_response(&mut self, r: String);
        fn glyph_register(&mut self, cp: u32, p: glyph_protocol::GlyphPayload, w: u8) -> Result<(), glyph_protocol::RegisterError>;
        fn glyph_clear(&mut self, cp: Option<u32>);
        fn glyph_query(&mut self, cp: u32);
        fn set_scp(&mut self, p: ScpCharPath, m: ScpUpdateMode);
    }
}
