//! What a keypress means: one table, and the chain that reads it.
//!
//! # Deliberately close to pua's
//!
//! pua is a rollup tree over processes and this is a rollup tree over directories, so the two
//! should feel like siblings: `j`/`k`, `←`/`→`, `*`, `z`, `g`/`G`, `s`/`S`, `/`, `?`, `q` and
//! `Esc` all mean here exactly what they mean there, and this file is a rewrite of pua's
//! `tui/keymap.rs` rather than an independent invention.
//!
//! It diverges exactly where the verbs do. pua kills one process and needs a key that asks
//! before signalling; pristine **marks a subtree** and then commits a batch, which is two verbs
//! rather than one. `space` marks (npkill's key for the same idea), `a` marks or clears
//! everything, and `x` — pua's one key that writes — commits what is marked. The keys pua
//! spends on sampling (`space` freezes, `r` re-samples) are free here, because a directory tree
//! does not tick.
//!
//! # Why a table rather than a `match`
//!
//! Three things have to agree about the keymap and drift apart the moment any of them is
//! written by hand: the dispatcher, the help overlay and the footer. [`KEYMAP`] is the single
//! statement of what is bound and all three read it, so a key that does something is a key the
//! help page documents by construction.
//!
//! # Routing is a chain of surfaces
//!
//! Overlay first, then the tree, then the globals. Spelled as a list of [`Surface`]s rather
//! than as branches in the dispatcher because of the guarantee attached to it: the tree can
//! never shadow a global key, which is one assertion over the table rather than a property
//! somebody has to keep noticing. An overlay is modal by *omission* — while one is up the
//! chain simply does not contain the tree.

use std::fmt;
use std::sync::LazyLock;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use crate::tree::Order;

/// Where a motion key wants the cursor.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Motion {
    /// One row back.
    Up,
    /// One row on.
    Down,
    /// A screenful back.
    PageUp,
    /// A screenful on.
    PageDown,
    /// The first row.
    Top,
    /// The last row.
    Bottom,
}

/// Which way round a cycle goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Turn {
    /// Forwards.
    Next,
    /// Backwards.
    Prev,
}

/// What a keypress asked for.
///
/// Separated from carrying it out so the keymap can be asserted directly: "`h` collapses" is
/// one assertion here rather than a terminal, a fixture tree and a rendered frame.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Action {
    /// `q` `Ctrl-c` — put the terminal back and go.
    Quit,
    /// A motion key — move the cursor, which moves the viewport with it.
    Cursor(Motion),
    /// `→` `l` `Enter` — open a row, or step into an open one.
    Expand,
    /// `←` `h` — close a row, or step out of a closed one.
    Collapse,
    /// `*` — open or close everything under the cursor at once.
    ToggleSubtree,
    /// `z` — close every open row, back to the roots.
    ///
    /// **Not a toggle**, where [`ToggleSubtree`](Self::ToggleSubtree) is: `*` is ambiguous on
    /// a partly-open subtree and this key is not, so it keeps the one meaning it has. An
    /// "open everything" companion is deliberately absent — one real home directory expands
    /// to 22,765 rows, and that is not a view anybody asked for.
    CollapseAll,
    /// `space` — mark the row's whole subtree, or unmark it.
    ///
    /// The key npkill uses for selecting a row, doing the thing npkill's flat list cannot: a
    /// mark on a collapsed row covers everything beneath it.
    Mark,
    /// `a` — mark everything, or clear the marks.
    ///
    /// Ambiguous on a partial selection in a way `space` is not, and resolved toward
    /// **clearing**: a reader who has marked forty directories and presses an unfamiliar key
    /// can afford to lose the selection and cannot afford to gain thirty more.
    MarkAll,
    /// `x` — remove what is marked. Asks first.
    ///
    /// pua's key for the one thing that writes, and the sentence in the help says that it
    /// asks: a reader scanning the page for a way to free space must not have to press it to
    /// find out whether it is armed. The key **asks** and never deletes; the only thing that
    /// commits is the dialog handing back what it was holding.
    Commit,
    /// `s` — the next sort key.
    CycleSort,
    /// `S` — the same key, upside down.
    ReverseSort,
    /// `1` `2` `3` — a sort key by position, derived from [`Order::ALL`].
    SortBy(Order),
    /// `/` — open the filter prompt.
    OpenFilter,
    /// A printable character, while the prompt has it.
    ///
    /// **Not in [`KEYMAP`]**, and it could not be: it stands for every character a terminal
    /// can report, which is not a list. It is also not a keybinding — typing `v` into a text
    /// field is content, not a command.
    Type(char),
    /// `Backspace` in the prompt.
    Erase,
    /// `Delete` in the prompt.
    EraseAhead,
    /// `Ctrl-u` — throw the prompt's line away.
    Wipe,
    /// `←` `→` `Home` `End` in the prompt.
    Caret(Motion),
    /// `Enter` in the prompt — apply the filter.
    Submit,
    /// `?` — show or hide the help.
    Help,
    /// `Esc` — step back one rung, and never quit.
    Back,
    /// `←` `→` on a confirmation — move the highlight between the two answers.
    ///
    /// Arrows and **not `Tab`**: a modal quietly redefining a key is worst in the one place a
    /// reader is being asked to be careful.
    Highlight(Turn),
    /// `Enter` on a confirmation — answer with whichever one is highlighted.
    ///
    /// It does not say *which* answer: the dialog holds both, so this is only "the one I am
    /// looking at". The highlight starts on cancel, so the key a reader presses to get rid of
    /// what is in front of them is the safe one.
    Answer,
    /// `↑` `↓` inside the help overlay.
    Scroll(Motion),
    /// A key with no meaning here, or a resize. Any event redraws, so this is genuinely
    /// nothing — the resize included, which needs only the frame.
    Ignore,
}

/// Which layer of the screen a binding belongs to.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Surface {
    /// Reserved everywhere. Small on purpose — every key here is one the tree may never take.
    Global,
    /// The rows themselves, which is where every key that acts on a directory lives.
    Tree,
    /// The help overlay's own keys, reachable only while it is up.
    Help,
    /// The filter prompt's, which are the one surface allowed to take a global key: while a
    /// text field has input there is no chain past it. See [`chain`].
    Prompt,
    /// The confirmation dialog's two answers.
    Confirm,
}

impl Surface {
    /// The heading this surface gets in the help overlay.
    #[must_use]
    pub fn title(self) -> &'static str {
        match self {
            Self::Global => "Everywhere",
            Self::Tree => "The tree",
            Self::Help => "This overlay",
            Self::Prompt => "The filter prompt",
            Self::Confirm => "A confirmation",
        }
    }
}

/// One key, with the modifier that distinguishes it from the bare version.
///
/// `Shift` is deliberately absent: a terminal reports `S` as `Char('S')`, so the shifted
/// letter is already a different [`KeyCode`]. Recording it as well would mean matching two
/// spellings of every capital.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Chord {
    /// The key.
    pub code: KeyCode,
    /// Whether Control was held.
    pub ctrl: bool,
}

impl Chord {
    const fn plain(code: KeyCode) -> Self {
        Self { code, ctrl: false }
    }

    const fn ctrl(letter: char) -> Self {
        Self {
            code: KeyCode::Char(letter),
            ctrl: true,
        }
    }

    /// What the reader actually pressed.
    #[must_use]
    pub fn of(key: KeyEvent) -> Self {
        Self {
            code: key.code,
            ctrl: key.modifiers.contains(KeyModifiers::CONTROL),
        }
    }
}

impl fmt::Display for Chord {
    /// How the help overlay spells this key.
    ///
    /// Rendered from the chord rather than written beside it, so a binding cannot advertise a
    /// key it does not answer to — which is the failure a hand-maintained help screen has by
    /// construction.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if self.ctrl {
            f.write_str("Ctrl-")?;
        }
        match self.code {
            KeyCode::Char(' ') => f.write_str("space"),
            KeyCode::Char(letter) => write!(f, "{letter}"),
            KeyCode::Up => f.write_str("↑"),
            KeyCode::Down => f.write_str("↓"),
            KeyCode::Left => f.write_str("←"),
            KeyCode::Right => f.write_str("→"),
            KeyCode::Enter => f.write_str("Enter"),
            KeyCode::Esc => f.write_str("Esc"),
            KeyCode::Home => f.write_str("Home"),
            KeyCode::End => f.write_str("End"),
            KeyCode::PageUp => f.write_str("PgUp"),
            KeyCode::PageDown => f.write_str("PgDn"),
            KeyCode::Backspace => f.write_str("Backspace"),
            KeyCode::Delete => f.write_str("Delete"),
            other => write!(f, "{other:?}"),
        }
    }
}

/// One row of the keymap: the keys that do a thing, and what the thing is.
#[derive(Clone, Debug)]
pub struct Binding {
    /// Which layer of the screen this binding belongs to.
    pub surface: Surface,
    /// Every key that produces this action, in the order the help lists them.
    pub chords: Vec<Chord>,
    /// The sentence the help overlay prints. Lower case and imperative, so the generated page
    /// reads as a list rather than as prose.
    pub what: &'static str,
    /// What the key asks for.
    pub action: Action,
}

impl Binding {
    /// The keys, spelled as the overlay spells them.
    #[must_use]
    pub fn keys(&self) -> String {
        self.chords
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(" ")
    }
}

fn bind(surface: Surface, chords: &[Chord], what: &'static str, action: Action) -> Binding {
    Binding {
        surface,
        chords: chords.to_vec(),
        what,
        action,
    }
}

const fn key(letter: char) -> Chord {
    Chord::plain(KeyCode::Char(letter))
}

/// Every binding pristine has, in help order.
///
/// Built at first use rather than written as a `const`, because the sort digits are *derived*
/// from [`Order::ALL`]: a fourth way to order a level arrives with its key already bound,
/// documented and dispatched.
static KEYMAP: LazyLock<Vec<Binding>> = LazyLock::new(build);

/// The whole keymap, for anything that renders or checks it.
#[must_use]
pub fn bindings() -> &'static [Binding] {
    &KEYMAP
}

fn build() -> Vec<Binding> {
    let mut map = globals();
    map.extend(tree_keys());
    map.extend(overlay_keys());
    map
}

/// The keys no surface may ever take.
fn globals() -> Vec<Binding> {
    use Surface::Global;
    vec![
        bind(Global, &[key('q'), Chord::ctrl('c')], "quit", Action::Quit),
        bind(Global, &[key('?')], "show or hide this help", Action::Help),
        bind(
            Global,
            &[key('/')],
            "filter by a regex over the whole path",
            Action::OpenFilter,
        ),
        bind(
            Global,
            &[Chord::plain(KeyCode::Esc)],
            "step back one level — never quits",
            Action::Back,
        ),
    ]
}

/// The keys that act on a directory. Every one of them belongs to the pane that has a cursor.
fn tree_keys() -> Vec<Binding> {
    let mut map = tree_motion();
    map.extend(tree_verbs());
    // One digit per order, derived rather than listed, so the key, the help row and the
    // dispatch for a fourth ordering all arrive together.
    for (nth, order) in Order::ALL.iter().enumerate() {
        map.push(bind(
            Surface::Tree,
            &[key(digit_for(nth))],
            match order {
                Order::Size => "sort by size — biggest subtree first",
                Order::Path => "sort by path",
                Order::Age => "sort by age — stalest first",
            },
            Action::SortBy(*order),
        ));
    }
    map
}

/// Moving the cursor, and moving the tree's own shape around it.
fn tree_motion() -> Vec<Binding> {
    use Surface::Tree;
    vec![
        bind(
            Tree,
            &[Chord::plain(KeyCode::Up), key('k')],
            "move up",
            Action::Cursor(Motion::Up),
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::Down), key('j')],
            "move down",
            Action::Cursor(Motion::Down),
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::PageUp), Chord::ctrl('u')],
            "up a page",
            Action::Cursor(Motion::PageUp),
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::PageDown), Chord::ctrl('d')],
            "down a page",
            Action::Cursor(Motion::PageDown),
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::Home), key('g')],
            "to the top",
            Action::Cursor(Motion::Top),
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::End), key('G')],
            "to the bottom",
            Action::Cursor(Motion::Bottom),
        ),
        bind(
            Tree,
            &[
                Chord::plain(KeyCode::Right),
                key('l'),
                Chord::plain(KeyCode::Enter),
            ],
            "open a row, or step into an open one",
            Action::Expand,
        ),
        bind(
            Tree,
            &[Chord::plain(KeyCode::Left), key('h')],
            "close a row, or step out of a closed one",
            Action::Collapse,
        ),
        bind(
            Tree,
            &[key('*')],
            "open or close the whole subtree",
            Action::ToggleSubtree,
        ),
        bind(
            Tree,
            &[key('z')],
            "close every open row, back to the roots",
            Action::CollapseAll,
        ),
    ]
}

/// What a reader does to what they have found.
fn tree_verbs() -> Vec<Binding> {
    use Surface::Tree;
    vec![
        bind(
            Tree,
            &[key(' ')],
            "mark this row's whole subtree, or unmark it",
            Action::Mark,
        ),
        bind(
            Tree,
            &[key('a')],
            "mark everything, or clear the marks",
            Action::MarkAll,
        ),
        // The only key here that writes, and the only sentence that has to say it asks.
        bind(
            Tree,
            &[key('x')],
            "delete what is marked — asks first",
            Action::Commit,
        ),
        bind(Tree, &[key('s')], "the next sort key", Action::CycleSort),
        bind(
            Tree,
            &[key('S')],
            "the same sort, upside down",
            Action::ReverseSort,
        ),
    ]
}

/// The three modal surfaces: the filter prompt, the help page, and a confirmation.
fn overlay_keys() -> Vec<Binding> {
    let mut map = prompt_keys();
    map.extend(help_keys());
    map.extend(confirm_keys());
    map
}

/// A text field's keys.
fn prompt_keys() -> Vec<Binding> {
    use Surface::Prompt;
    vec![
        // ---- The filter prompt ---------------------------------------
        //
        // Every key here is one a *text field* needs, which is why this surface is the one
        // place a global may be shadowed: while the prompt is up there is no chain past it,
        // so `Ctrl-c` and `Esc` are re-bound here rather than reached through the globals.
        // Printable characters are not in this list — see [`Action::Type`].
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Enter)],
            "apply the filter",
            Action::Submit,
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Backspace)],
            "rub out the character before the caret",
            Action::Erase,
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Delete)],
            "rub out the character after it",
            Action::EraseAhead,
        ),
        bind(
            Prompt,
            &[Chord::ctrl('u')],
            "throw the line away",
            Action::Wipe,
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Left)],
            "caret left",
            Action::Caret(Motion::Up),
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Right)],
            "caret right",
            Action::Caret(Motion::Down),
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Home)],
            "caret to the start",
            Action::Caret(Motion::Top),
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::End)],
            "caret to the end",
            Action::Caret(Motion::Bottom),
        ),
        bind(
            Prompt,
            &[Chord::plain(KeyCode::Esc)],
            "close the prompt, leaving the filter as it was",
            Action::Back,
        ),
        bind(
            Prompt,
            &[Chord::ctrl('c')],
            "quit — reserved everywhere, this surface included",
            Action::Quit,
        ),
    ]
}

/// Scrolling a document, and nothing else.
fn help_keys() -> Vec<Binding> {
    use Surface::Help;
    vec![
        // ---- The help overlay ----------------------------------------
        //
        // Scrolling only. `Esc` and `?` close it through their global bindings, which is what
        // stops this surface from being a second place those two keys are defined.
        bind(
            Help,
            &[Chord::plain(KeyCode::Up), key('k')],
            "scroll up",
            Action::Scroll(Motion::Up),
        ),
        bind(
            Help,
            &[Chord::plain(KeyCode::Down), key('j')],
            "scroll down",
            Action::Scroll(Motion::Down),
        ),
        bind(
            Help,
            &[Chord::plain(KeyCode::PageUp)],
            "scroll up a page",
            Action::Scroll(Motion::PageUp),
        ),
        bind(
            Help,
            &[Chord::plain(KeyCode::PageDown)],
            "scroll down a page",
            Action::Scroll(Motion::PageDown),
        ),
        bind(
            Help,
            &[Chord::plain(KeyCode::Home), key('g')],
            "to the top",
            Action::Scroll(Motion::Top),
        ),
        bind(
            Help,
            &[Chord::plain(KeyCode::End), key('G')],
            "to the bottom",
            Action::Scroll(Motion::Bottom),
        ),
    ]
}

/// Two answers, chosen rather than named.
fn confirm_keys() -> Vec<Binding> {
    use Surface::Confirm;
    vec![
        // ---- A confirmation ------------------------------------------
        //
        // Two answers, chosen rather than named: `←` and `→` move the highlight and `Enter`
        // takes the highlighted one. `Esc` cancels through its global binding. `y` and `n`
        // are deliberately unbound — a key that acts while being undocumented is the failure
        // this table exists to make impossible, and the box shows the two answers.
        bind(
            Confirm,
            &[Chord::plain(KeyCode::Left)],
            "highlight cancel, the left-hand answer",
            Action::Highlight(Turn::Prev),
        ),
        bind(
            Confirm,
            &[Chord::plain(KeyCode::Right)],
            "highlight delete",
            Action::Highlight(Turn::Next),
        ),
        bind(
            Confirm,
            &[Chord::plain(KeyCode::Enter)],
            "answer with the highlighted one",
            Action::Answer,
        ),
    ]
}

/// The digit key for the `nth` member of a positional group, counting from 1.
///
/// Falls back to a key nobody presses rather than wrapping round to `1`, which would silently
/// give a tenth ordering the first one's key.
fn digit_for(nth: usize) -> char {
    u32::try_from(nth)
        .ok()
        .and_then(|nth| char::from_digit(nth + 1, 10))
        .unwrap_or('\0')
}

/// Which overlay is up, if any.
///
/// Named rather than a `bool`, because they differ in the one way routing cares about: help is
/// a document laid over the screen and the globals still reach past it, while the prompt is a
/// **text field** where every printable key belongs to the field.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Overlay {
    /// The generated key reference.
    Help,
    /// The filter's text field.
    Prompt,
    /// A confirmation dialog. Modal the way the other two are — by leaving the tree out of the
    /// chain — rather than by a rule of its own: the globals still reach past it, which keeps
    /// `q` and `Ctrl-c` the way out everywhere, and a reader who reached the question by
    /// accident must not have to guess that it took quitting away.
    Confirm,
}

/// The surfaces a key is offered to, in order.
///
/// An overlay is modal by omission: while one is up the tree is simply not in the chain, so no
/// tree key can reach the tree from behind it. The prompt is the one surface that comes
/// *before* the globals, because a text field owns every key it needs — including `Esc`, and
/// including the printable letters that would otherwise be tree commands.
#[must_use]
pub fn chain(overlay: Option<Overlay>) -> Vec<Surface> {
    match overlay {
        Some(Overlay::Prompt) => vec![Surface::Prompt],
        Some(Overlay::Help) => vec![Surface::Help, Surface::Global],
        Some(Overlay::Confirm) => vec![Surface::Confirm, Surface::Global],
        None => vec![Surface::Tree, Surface::Global],
    }
}

/// What one terminal event means, here and now.
#[must_use]
pub fn action_for(event: &Event, overlay: Option<Overlay>) -> Action {
    let Event::Key(key) = event else {
        return Action::Ignore;
    };
    // Terminals that speak the kitty protocol report releases as well as presses. Without
    // this every key would fire twice.
    if key.kind == KeyEventKind::Release {
        return Action::Ignore;
    }

    let chord = Chord::of(*key);
    if let Some(action) = chain(overlay)
        .iter()
        .find_map(|&surface| lookup(surface, chord))
    {
        return action;
    }

    // The prompt's catch-all, and the reason it is here rather than in the table: it stands
    // for every character a terminal can report. A modifier rules it out — `Ctrl-x` in a text
    // field is a command nobody bound, not an `x` — which is what keeps the explicit prompt
    // chords above reachable.
    match (overlay, chord) {
        (
            Some(Overlay::Prompt),
            Chord {
                code: KeyCode::Char(character),
                ctrl: false,
            },
        ) => Action::Type(character),
        _ => Action::Ignore,
    }
}

/// What this chord does on this surface, if anything.
fn lookup(surface: Surface, chord: Chord) -> Option<Action> {
    bindings()
        .iter()
        .find(|binding| binding.surface == surface && binding.chords.contains(&chord))
        .map(|binding| binding.action)
}

/// The help page, as headed groups of `(keys, sentence)`.
///
/// Generated from the table rather than written, which is what keeps it honest: a binding
/// added without a sentence does not compile, and a sentence with no binding cannot exist.
#[must_use]
pub fn help() -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let surfaces = [
        Surface::Global,
        Surface::Tree,
        Surface::Prompt,
        Surface::Confirm,
        Surface::Help,
    ];
    surfaces
        .into_iter()
        .map(|surface| {
            let rows = bindings()
                .iter()
                .filter(|binding| binding.surface == surface)
                .map(|binding| (binding.keys(), binding.what))
                .collect();
            (surface.title(), rows)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{Action, Chord, Motion, Overlay, Surface, action_for, bindings, help, lookup};
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn letter(letter: char) -> Event {
        press(KeyCode::Char(letter))
    }

    #[test]
    fn the_tree_never_shadows_a_global_key() {
        // The guarantee behind the chain being a list of surfaces rather than a pile of
        // conditions: `q` means quit wherever a reader presses it, and no future binding can
        // quietly take it away on one screen.
        for binding in bindings() {
            if binding.surface == Surface::Global {
                continue;
            }
            for chord in &binding.chords {
                let shadowed =
                    binding.surface != Surface::Prompt && lookup(Surface::Global, *chord).is_some();
                assert!(
                    !shadowed,
                    "{:?} takes {chord}, which is global",
                    binding.surface
                );
            }
        }
    }

    #[test]
    fn every_binding_has_a_sentence_and_at_least_one_key() {
        for binding in bindings() {
            assert!(!binding.chords.is_empty(), "{binding:?} binds nothing");
            assert!(!binding.what.is_empty(), "{binding:?} says nothing");
            assert!(
                binding.what.starts_with(|c: char| c.is_lowercase()),
                "{:?} is not a lower-case imperative",
                binding.what
            );
        }
    }

    #[test]
    fn no_surface_binds_one_key_to_two_things() {
        for binding in bindings() {
            for chord in &binding.chords {
                let claimants = bindings()
                    .iter()
                    .filter(|other| {
                        other.surface == binding.surface && other.chords.contains(chord)
                    })
                    .count();
                assert_eq!(
                    claimants, 1,
                    "{chord} is bound twice on {:?}",
                    binding.surface
                );
            }
        }
    }

    #[test]
    fn the_help_page_lists_every_binding_there_is() {
        let listed: usize = help().iter().map(|(_, rows)| rows.len()).sum();
        assert_eq!(listed, bindings().len());
    }

    #[test]
    fn a_tree_key_cannot_reach_the_tree_from_behind_an_overlay() {
        assert_eq!(action_for(&letter('x'), None), Action::Commit);
        // The dangerous key, in particular: a reader reading the help page must not be able
        // to delete their marked batch by pressing the key their eye is on.
        assert_eq!(
            action_for(&letter('x'), Some(Overlay::Help)),
            Action::Ignore
        );
        assert_eq!(
            action_for(&letter('x'), Some(Overlay::Confirm)),
            Action::Ignore
        );
    }

    #[test]
    fn a_printable_key_is_content_while_the_prompt_is_up() {
        assert_eq!(
            action_for(&letter('x'), Some(Overlay::Prompt)),
            Action::Type('x')
        );
        assert_eq!(
            action_for(&letter(' '), Some(Overlay::Prompt)),
            Action::Type(' ')
        );
        // …and a chord is not content, so the prompt's own editing keys stay reachable.
        assert_eq!(
            action_for(
                &Event::Key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL)),
                Some(Overlay::Prompt)
            ),
            Action::Wipe
        );
    }

    #[test]
    fn quitting_is_reachable_from_every_surface_including_the_text_field() {
        for overlay in [
            None,
            Some(Overlay::Help),
            Some(Overlay::Confirm),
            Some(Overlay::Prompt),
        ] {
            let ctrl_c = Event::Key(KeyEvent::new(KeyCode::Char('c'), KeyModifiers::CONTROL));
            assert_eq!(action_for(&ctrl_c, overlay), Action::Quit, "{overlay:?}");
        }
    }

    #[test]
    fn a_ctrl_chord_is_not_the_bare_letter() {
        // `Ctrl-d` is half a page and `d` is nothing at all; matching on the code alone
        // would make those the same key, recoverable only by checking the modifier first.
        assert_eq!(
            action_for(
                &Event::Key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::CONTROL)),
                None
            ),
            Action::Cursor(Motion::PageDown)
        );
        assert_eq!(action_for(&letter('d'), None), Action::Ignore);
    }

    #[test]
    fn a_key_release_is_not_a_second_press() {
        let release = Event::Key(KeyEvent::new_with_kind_and_state(
            KeyCode::Char('x'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
            KeyEventState::NONE,
        ));
        assert_eq!(action_for(&release, None), Action::Ignore);
    }

    #[test]
    fn a_chord_spells_itself_the_way_the_help_page_prints_it() {
        assert_eq!(Chord::plain(KeyCode::Char(' ')).to_string(), "space");
        assert_eq!(Chord::ctrl('u').to_string(), "Ctrl-u");
        assert_eq!(Chord::plain(KeyCode::Up).to_string(), "↑");
    }
}
