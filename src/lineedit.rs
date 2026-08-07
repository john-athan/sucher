// A single-line text buffer with a real cursor, for the browser's inline
// rename/create prompt (ADR 0017 D4).
//
// The browser already has two text surfaces, and both are append-only: the `/`
// filter and the recursive-search query grow at the end and shrink with
// Backspace. That is enough for a query, which is short and cheap to retype. A
// rename is the case it is not enough for: the field opens PRE-FILLED with an
// existing name, and the edit the user came to make is usually in the middle of
// it. So this module adds the one thing those two lack, a cursor, and nothing
// else.
//
// The filter and the search query are deliberately NOT converted to use it, and
// that is a decision rather than an omission:
//
//   * Search binds `Right` to "open the selected hit" (ADR 0007). A horizontal
//     cursor needs `Left`/`Right`, so the two cannot coexist in one key map, and
//     the binding search already has is worth more than a cursor in a query
//     nobody edits in the middle.
//   * The filter has the keys free but no need. Its buffer is a throwaway query
//     that the user is watching narrow a listing as they type; there is no
//     pre-filled text to go back into, so a cursor would add state without
//     adding anything visible.
//
// Saying so here is the point: without it, "unify the three text buffers" looks
// like an obvious cleanup, and doing it would silently break search's `Right`.
//
// Pure: no ratatui, no crossterm, no filesystem. The caller translates key
// events into these calls and reads [`LineEdit::split`] back out to draw.

/// A line of text plus the position being edited.
///
/// **The cursor counts CHARACTERS, not bytes.** File names routinely hold
/// multi-byte UTF-8 (`café.txt`, an emoji in a folder name), and a byte cursor
/// in a `String` is one arithmetic slip away from slicing through the middle of
/// a code point, which in Rust is a panic rather than mojibake. Counting
/// characters makes that impossible to express: every position the cursor can
/// hold is a character boundary by construction, and the single conversion to a
/// byte offset lives in [`LineEdit::byte_of`], where it is derived from
/// `char_indices` and so is always a boundary too.
///
/// The cost is that each edit walks the string to find that offset. A file name
/// is a few dozen characters and an edit happens once per keystroke, so the walk
/// is free, and it buys a type that cannot panic on any input.
///
/// A character is not always a whole grapheme (a combining accent, an emoji
/// built from a ZWJ sequence), so the cursor can land between the two halves of
/// such a cluster. That is accepted: fixing it needs a segmentation table, this
/// tool has no such dependency, and the failure mode is a cursor one position
/// off rather than a corrupted name or a crash.
#[derive(Debug, Default)]
pub struct LineEdit {
    text: String,
    /// Character index in `text`, from 0 to its character count inclusive. The
    /// upper end is a real position: it is where a fresh prompt starts and where
    /// typing at the end of a name happens.
    cursor: usize,
}

impl LineEdit {
    /// An empty buffer, which has exactly one cursor position.
    pub fn new() -> Self {
        Self::default()
    }

    /// A buffer pre-filled with `text` and the cursor at character `cursor`.
    ///
    /// The position is clamped rather than rejected, because the callers that
    /// compute one (the rename prompt's "end of the stem" rule) derive it from
    /// the same name and would only ever be out of range through a bug. Clamping
    /// turns that bug into a cursor at the end of the line instead of a panic in
    /// front of the user.
    pub fn with_text(text: &str, cursor: usize) -> Self {
        let mut edit = LineEdit {
            text: text.to_string(),
            cursor: 0,
        };
        edit.cursor = cursor.min(edit.char_count());
        edit
    }

    /// The text as typed so far.
    pub fn text(&self) -> &str {
        &self.text
    }

    /// The cursor's position, counted in characters.
    pub fn cursor(&self) -> usize {
        self.cursor
    }

    /// The buffer split at the cursor: what lies before it, the character under
    /// it, and what follows.
    ///
    /// The middle piece is empty exactly when the cursor is at the end of the
    /// line, which is the case a renderer has to treat differently: there is no
    /// character to highlight, so it has to draw a block instead. Returning the
    /// three pieces from here rather than exposing the byte arithmetic keeps the
    /// one place that can slice a `String` inside this module.
    pub fn split(&self) -> (&str, &str, &str) {
        let head = self.byte_of(self.cursor());
        let under = self.text[head..].chars().next();
        let tail = head + under.map_or(0, char::len_utf8);
        (
            &self.text[..head],
            &self.text[head..tail],
            &self.text[tail..],
        )
    }

    /// Insert `c` at the cursor and step over it, which is what typing means.
    pub fn insert(&mut self, c: char) {
        let at = self.byte_of(self.cursor);
        self.text.insert(at, c);
        self.cursor += 1;
    }

    /// Remove the character BEFORE the cursor. A no-op at the start of the line,
    /// where there is nothing behind to remove.
    pub fn backspace(&mut self) {
        if self.cursor == 0 {
            return;
        }
        self.cursor -= 1;
        let at = self.byte_of(self.cursor);
        self.text.remove(at);
    }

    /// Remove the character UNDER the cursor, leaving the cursor where it is. A
    /// no-op at the end of the line, where there is nothing under it.
    pub fn delete(&mut self) {
        if self.cursor >= self.char_count() {
            return;
        }
        let at = self.byte_of(self.cursor);
        self.text.remove(at);
    }

    /// One character left, stopping at the start.
    pub fn left(&mut self) {
        self.cursor = self.cursor.saturating_sub(1);
    }

    /// One character right, stopping one past the last character, which is the
    /// position typing appends from.
    pub fn right(&mut self) {
        self.cursor = (self.cursor + 1).min(self.char_count());
    }

    pub fn home(&mut self) {
        self.cursor = 0;
    }

    pub fn end(&mut self) {
        self.cursor = self.char_count();
    }

    /// How many characters the buffer holds, which is also its last cursor
    /// position.
    fn char_count(&self) -> usize {
        self.text.chars().count()
    }

    /// The byte offset of character `chars`, or the end of the string when the
    /// index is the one-past-the-end position.
    ///
    /// Every offset this returns comes from `char_indices` or from `len()`, so
    /// it is always a character boundary and the `String` insert/remove calls
    /// built on it can never panic.
    fn byte_of(&self, chars: usize) -> usize {
        self.text
            .char_indices()
            .nth(chars)
            .map_or(self.text.len(), |(at, _)| at)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The whole state in one assertion: what the buffer holds and where the
    /// cursor is, so a test reads as a picture of the line.
    fn state(edit: &LineEdit) -> (&str, usize) {
        (edit.text(), edit.cursor())
    }

    #[test]
    fn an_empty_buffer_has_exactly_one_position() {
        let mut edit = LineEdit::new();
        assert_eq!(state(&edit), ("", 0));
        assert_eq!(edit.split(), ("", "", ""));
        // Every motion and every removal is a no-op on nothing at all, rather
        // than an underflow.
        edit.left();
        edit.right();
        edit.home();
        edit.end();
        edit.backspace();
        edit.delete();
        assert_eq!(state(&edit), ("", 0));
    }

    #[test]
    fn typing_inserts_at_the_cursor_and_advances_it() {
        let mut edit = LineEdit::new();
        for c in "abc".chars() {
            edit.insert(c);
        }
        assert_eq!(state(&edit), ("abc", 3));
        // Inserting in the middle pushes the tail along rather than overwriting.
        edit.home();
        edit.right();
        edit.insert('X');
        assert_eq!(state(&edit), ("aXbc", 2));
    }

    #[test]
    fn backspace_removes_behind_the_cursor_and_stops_at_the_start() {
        let mut edit = LineEdit::with_text("abc", 3);
        edit.backspace();
        assert_eq!(state(&edit), ("ab", 2));
        edit.home();
        // Nothing lies behind position 0, so the buffer is untouched.
        edit.backspace();
        assert_eq!(state(&edit), ("ab", 0));
    }

    #[test]
    fn delete_removes_under_the_cursor_and_stops_at_the_end() {
        let mut edit = LineEdit::with_text("abc", 1);
        edit.delete();
        // The cursor does not move: the tail slid under it.
        assert_eq!(state(&edit), ("ac", 1));
        edit.end();
        // Nothing sits under the end-of-line position, so the buffer is
        // untouched rather than eating the last character.
        edit.delete();
        assert_eq!(state(&edit), ("ac", 2));
    }

    #[test]
    fn the_cursor_stops_at_both_ends() {
        let mut edit = LineEdit::with_text("ab", 0);
        edit.left();
        assert_eq!(edit.cursor(), 0);
        edit.right();
        edit.right();
        assert_eq!(edit.cursor(), 2);
        // One past the last character IS a position: it is where typing appends.
        edit.right();
        assert_eq!(edit.cursor(), 2);
        edit.insert('c');
        assert_eq!(state(&edit), ("abc", 3));
    }

    #[test]
    fn home_and_end_jump_to_the_ends() {
        let mut edit = LineEdit::with_text("notes.md", 5);
        edit.home();
        assert_eq!(edit.cursor(), 0);
        edit.end();
        assert_eq!(edit.cursor(), 8);
    }

    #[test]
    fn a_prefilled_cursor_past_the_end_is_clamped_rather_than_fatal() {
        let edit = LineEdit::with_text("ab", 99);
        assert_eq!(state(&edit), ("ab", 2));
        // And the ordinary case is kept verbatim.
        assert_eq!(state(&LineEdit::with_text("notes.md", 5)), ("notes.md", 5));
    }

    #[test]
    fn split_names_the_character_under_the_cursor() {
        let edit = LineEdit::with_text("notes.md", 5);
        assert_eq!(edit.split(), ("notes", ".", "md"));
        assert_eq!(LineEdit::with_text("ab", 0).split(), ("", "a", "b"));
        // At the end there is nothing under the cursor, which is what tells a
        // renderer to draw a block instead of highlighting a character.
        assert_eq!(LineEdit::with_text("ab", 2).split(), ("ab", "", ""));
    }

    /// The reason the cursor counts characters rather than bytes. Every one of
    /// these would slice through a code point under byte arithmetic, and in Rust
    /// that is a panic in front of the user, on a file name they did nothing
    /// wrong to own.
    #[test]
    fn multibyte_names_move_and_edit_a_whole_character_at_a_time() {
        // `café.txt` is 8 characters and 9 bytes.
        let mut edit = LineEdit::with_text("café.txt", 8);
        assert_eq!(edit.cursor(), 8);
        edit.home();
        for _ in 0..3 {
            edit.right();
        }
        // One press per CHARACTER, so three of them land in front of the `é`.
        assert_eq!(edit.split(), ("caf", "é", ".txt"));
        // Deleting it takes the whole two-byte character, not half of it.
        edit.delete();
        assert_eq!(state(&edit), ("caf.txt", 3));
        // And backspacing over one from behind does the same.
        let mut edit = LineEdit::with_text("café", 4);
        edit.backspace();
        assert_eq!(state(&edit), ("caf", 3));
    }

    #[test]
    fn an_emoji_is_one_position_and_survives_an_edit_beside_it() {
        // A four-byte character, the widest a single `char` gets.
        let mut edit = LineEdit::with_text("a🎉b", 3);
        assert_eq!(edit.split(), ("a🎉b", "", ""));
        edit.left();
        assert_eq!(edit.split(), ("a🎉", "b", ""));
        edit.left();
        assert_eq!(edit.split(), ("a", "🎉", "b"));
        // Inserting in front of it leaves it intact.
        edit.insert('-');
        assert_eq!(state(&edit), ("a-🎉b", 2));
        edit.backspace();
        assert_eq!(state(&edit), ("a🎉b", 1));
        // And removing it removes all four bytes at once.
        edit.delete();
        assert_eq!(state(&edit), ("ab", 1));
    }

    /// Typing a multi-byte character is the same operation from the other side:
    /// the buffer grows by a whole character and the cursor by one position.
    #[test]
    fn typing_a_multibyte_character_advances_one_position() {
        let mut edit = LineEdit::new();
        edit.insert('n');
        edit.insert('ö');
        edit.insert('t');
        assert_eq!(state(&edit), ("nöt", 3));
        assert_eq!(edit.text().len(), 4); // four BYTES, three positions
        edit.left();
        edit.left();
        assert_eq!(edit.split(), ("n", "ö", "t"));
    }
}
