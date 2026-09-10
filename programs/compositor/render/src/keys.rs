//! What a key press means, on the one layout this desktop has.
//!
//! The kernel hands out evdev triples and translates nothing, which is
//! right: there is no fact about a key press that a Helios encoding
//! could carry and evdev could not. Somebody has to decide what a
//! keycode *means* though, and for a terminal that means bytes on the
//! shell's standard input. This is where that decision lives.
//!
//! The layout is a table rather than a match because evdev's codes are
//! the keys' own positions: `0x02..=0x35` are the four rows of a PC
//! keyboard in the order they sit on it, so the index into the table is
//! the key, and a second layout would be a second table rather than a
//! second branch on every code.

use helios_api::input::codes;

/// The first code the layout table covers.
const FIRST_CODE: u16 = codes::KEY_1;

/// The last code the layout table covers.
const LAST_CODE: u16 = codes::KEY_SLASH;

/// A key that produces no character on its own: a modifier, or one this
/// layout has nothing to say about.
const NONE: (char, char) = ('\0', '\0');

/// The United States layout, unshifted and shifted, from
/// [`FIRST_CODE`] to [`LAST_CODE`].
const LAYOUT: [(char, char); (LAST_CODE - FIRST_CODE + 1) as usize] = [
    ('1', '!'),
    ('2', '@'),
    ('3', '#'),
    ('4', '$'),
    ('5', '%'),
    ('6', '^'),
    ('7', '&'),
    ('8', '*'),
    ('9', '('),
    ('0', ')'),
    ('-', '_'),
    ('=', '+'),
    ('\u{08}', '\u{08}'),
    ('\t', '\t'),
    ('q', 'Q'),
    ('w', 'W'),
    ('e', 'E'),
    ('r', 'R'),
    ('t', 'T'),
    ('y', 'Y'),
    ('u', 'U'),
    ('i', 'I'),
    ('o', 'O'),
    ('p', 'P'),
    ('[', '{'),
    (']', '}'),
    ('\n', '\n'),
    NONE,
    ('a', 'A'),
    ('s', 'S'),
    ('d', 'D'),
    ('f', 'F'),
    ('g', 'G'),
    ('h', 'H'),
    ('j', 'J'),
    ('k', 'K'),
    ('l', 'L'),
    (';', ':'),
    ('\'', '"'),
    ('`', '~'),
    NONE,
    ('\\', '|'),
    ('z', 'Z'),
    ('x', 'X'),
    ('c', 'C'),
    ('v', 'V'),
    ('b', 'B'),
    ('n', 'N'),
    ('m', 'M'),
    (',', '<'),
    ('.', '>'),
    ('/', '?'),
];

/// The modifiers a keyboard is holding, and what a press means under
/// them.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Keyboard {
    shift: bool,
    control: bool,
    capitals: bool,
}

impl Keyboard {
    pub const fn new() -> Self {
        Self {
            shift: false,
            control: false,
            capitals: false,
        }
    }

    /// Take one `EV_KEY` event and report the bytes it produces.
    ///
    /// `value` is evdev's: zero for a release, one for a press, two for
    /// an auto-repeat, which produces the same bytes a press does
    /// because that is what repeating a key is for.
    pub fn press(&mut self, code: u16, value: i32) -> Option<char> {
        let held = value != 0;
        match code {
            codes::KEY_LEFTSHIFT | codes::KEY_RIGHTSHIFT => {
                self.shift = held;
                return None;
            }
            codes::KEY_LEFTCTRL | codes::KEY_RIGHTCTRL => {
                self.control = held;
                return None;
            }
            codes::KEY_CAPSLOCK => {
                // A lock toggles on the press and does nothing on the
                // release, unlike a modifier which is held.
                if held && value == 1 {
                    self.capitals = !self.capitals;
                }
                return None;
            }
            _ => {}
        }
        if !held {
            return None;
        }
        let character = match code {
            codes::KEY_SPACE => ' ',
            code if (FIRST_CODE..=LAST_CODE).contains(&code) => {
                let (plain, shifted) = LAYOUT[(code - FIRST_CODE) as usize];
                if plain == '\0' {
                    return None;
                }
                // Caps lock is not a shift: it capitalises letters and
                // leaves the digit row alone, which is the whole of what
                // makes the two different keys.
                if self.shift {
                    shifted
                } else if self.capitals && plain.is_ascii_alphabetic() {
                    plain.to_ascii_uppercase()
                } else {
                    plain
                }
            }
            _ => return None,
        };
        if self.control {
            // Control turns a letter into the control byte of the same
            // rank, which is what the shell reads as an interrupt or an
            // end of file.
            return character
                .is_ascii_alphabetic()
                .then(|| char::from(character.to_ascii_uppercase() as u8 & 0x1f));
        }
        Some(character)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn typed(keyboard: &mut Keyboard, code: u16) -> Option<char> {
        let pressed = keyboard.press(code, 1);
        keyboard.press(code, 0);
        pressed
    }

    #[test]
    fn a_letter_is_itself_and_a_release_produces_nothing() {
        let mut keyboard = Keyboard::new();
        assert_eq!(keyboard.press(codes::KEY_H, 1), Some('h'));
        assert_eq!(keyboard.press(codes::KEY_H, 0), None);
    }

    #[test]
    fn an_auto_repeat_types_the_key_again() {
        let mut keyboard = Keyboard::new();
        assert_eq!(keyboard.press(codes::KEY_H, 2), Some('h'));
    }

    #[test]
    fn shift_is_held_across_the_key_it_applies_to() {
        let mut keyboard = Keyboard::new();
        assert_eq!(keyboard.press(codes::KEY_LEFTSHIFT, 1), None);
        assert_eq!(keyboard.press(codes::KEY_H, 1), Some('H'));
        assert_eq!(keyboard.press(codes::KEY_2, 1), Some('@'));
        keyboard.press(codes::KEY_LEFTSHIFT, 0);
        assert_eq!(keyboard.press(codes::KEY_H, 1), Some('h'));
    }

    #[test]
    fn caps_lock_capitalises_letters_and_leaves_the_digits_alone() {
        let mut keyboard = Keyboard::new();
        typed(&mut keyboard, codes::KEY_CAPSLOCK);
        assert_eq!(typed(&mut keyboard, codes::KEY_H), Some('H'));
        assert_eq!(typed(&mut keyboard, codes::KEY_2), Some('2'));
        typed(&mut keyboard, codes::KEY_CAPSLOCK);
        assert_eq!(typed(&mut keyboard, codes::KEY_H), Some('h'));
    }

    #[test]
    fn control_and_a_letter_are_the_control_byte_of_that_rank() {
        let mut keyboard = Keyboard::new();
        keyboard.press(codes::KEY_LEFTCTRL, 1);
        assert_eq!(keyboard.press(codes::KEY_C, 1), Some('\u{03}'));
        assert_eq!(keyboard.press(codes::KEY_D, 1), Some('\u{04}'));
        // Control and a digit is nothing this terminal sends.
        assert_eq!(keyboard.press(codes::KEY_2, 1), None);
    }

    #[test]
    fn the_keys_a_shell_needs_are_the_bytes_it_expects() {
        let mut keyboard = Keyboard::new();
        assert_eq!(typed(&mut keyboard, codes::KEY_ENTER), Some('\n'));
        assert_eq!(typed(&mut keyboard, codes::KEY_SPACE), Some(' '));
        assert_eq!(typed(&mut keyboard, codes::KEY_BACKSPACE), Some('\u{08}'));
        assert_eq!(typed(&mut keyboard, codes::KEY_TAB), Some('\t'));
    }

    #[test]
    fn a_key_this_layout_has_nothing_for_types_nothing() {
        let mut keyboard = Keyboard::new();
        assert_eq!(typed(&mut keyboard, codes::KEY_F1), None);
        assert_eq!(typed(&mut keyboard, codes::KEY_LEFTALT), None);
    }
}
