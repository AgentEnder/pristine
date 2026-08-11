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
//!
//! # The pointer is a second table, for the same reason
//!
//! [`POINTER`] is to a mouse event what [`KEYMAP`] is to a keystroke, and it is a table for
//! the argument written above rather than by analogy: the dispatcher, the help overlay and
//! the guarantee that no gesture acts undocumented all read it. What it does *not* need is
//! the chain — a press has coordinates, so "which surface is this" is answered by
//! [`super::render::hit`] geometrically, and restating the order here would be two rules that
//! have to agree.

use std::fmt;
use std::sync::LazyLock;

use ratatui::crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

use super::render::{Spot, Zone};
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
    /// `1` `2` `3`, or a click on a column heading — order a level by that key.
    ///
    /// Reverses when it names the order already in force, and starts a *new* column the right
    /// way up: see [`super::state::View::sort_by`].
    SortBy(Order),
    /// A click on a row's name — put the cursor on that directory.
    ///
    /// By [`NodeId`](crate::tree::NodeId) and never by row index; see [`Spot::Row`].
    Select(crate::tree::NodeId),
    /// A click on a row's `▸` — open that row, or close it.
    ///
    /// The one thing `→` and `←` between them do, reached with one gesture and without the
    /// cursor having to be there first.
    OpenRow(crate::tree::NodeId),
    /// A click on a row's `[ ]` — mark that row's subtree, or unmark it.
    ///
    /// `space` for a reader who is pointing. pristine draws a box on every row, and a box
    /// that cannot be pressed is a lie the screen tells about itself.
    MarkRow(crate::tree::NodeId),
    /// A double click on a row — price everything under it that carries no price.
    ///
    /// The expensive thing a reader wants on one specific subtree, which is what
    /// `--breakdown-under` is on the command line. A double click is the gesture for it
    /// because it is the one that says "this one, in particular".
    Price(crate::tree::NodeId),
    /// The wheel over the tree — move the viewport, taking the cursor with it.
    ///
    /// Distinct from [`Cursor`](Self::Cursor), which moves the cursor and lets the viewport
    /// follow: this is the other way round.
    ScrollRows(Motion),
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
            // Each sentence says the key turns its own order upside down when pressed again,
            // because it does — the digits and a click on the heading are one door, and a
            // reader who does not know that would press `S` and reverse whatever `s` last
            // left in force instead.
            match order {
                Order::Size => "sort by size, biggest subtree first — again to reverse",
                Order::Path => "sort by path — again to reverse",
                Order::Age => "sort by age, stalest first — again to reverse",
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

// ---- the pointer ----------------------------------------------------------------------

/// What the loop has judged one mouse event to be.
///
/// Judgements about *previous* events, which the one in hand cannot see — whether a press has
/// already moved, whether one landed on this spot a moment ago — so they are made by
/// [`super::Pointer`] and handed here. That keeps [`pointer()`] a pure function of one gesture
/// and one spot, which is what makes the whole table assertable without a terminal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Gesture {
    /// A press, or the pointer merely passing over. It **aims and no more**.
    ///
    /// The rule the deferred click forced: at the moment the button goes down there is no way
    /// to tell a click from a drag, so a press that acted would re-sort the tree under a hand
    /// that was about to select from it. See [`finish`].
    Aim,
    /// A press and a release, with no movement between them — the click, at last.
    Click,
    /// Two clicks on one **row**, close enough together to be one gesture.
    Double,
    /// One turn of the wheel.
    ///
    /// The direction belongs to the *action* rather than to the row of the table: the wheel
    /// does the same thing over a spot whichever way it turns, so both turns are one row and
    /// [`Gesture::same`] is what keys them together.
    Wheel(Motion),
}

impl Gesture {
    /// Whether this is the gesture a table row describes.
    ///
    /// By kind rather than by value, which matters for exactly one variant: the two wheel
    /// turns are one row of the table, and the row has to be written with *some* direction
    /// in it.
    fn same(self, row: Self) -> bool {
        std::mem::discriminant(&self) == std::mem::discriminant(&row)
    }

    /// Which way the wheel turned, if it was the wheel.
    fn motion(self) -> Option<Motion> {
        match self {
            Self::Wheel(motion) => Some(motion),
            _ => None,
        }
    }
}

impl fmt::Display for Gesture {
    /// How the help overlay spells this gesture.
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Aim => "point at",
            Self::Click => "click",
            Self::Double => "double-click",
            Self::Wheel(_) => "wheel over",
        })
    }
}

/// What a gesture landed on, as the table names it — a [`Spot`] with the identity taken out.
///
/// The identity is what the *action* needs and what the table must not carry: a row of
/// [`POINTER`] says "a click on a name selects", and which directory is a fact about the
/// press rather than about the rule.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Target {
    /// A column heading.
    Heading,
    /// A row's mark box.
    Box,
    /// A row's expand indicator.
    Indicator,
    /// A row's name, and the space around it.
    Name,
    /// A row, whichever part of it — what a gesture that is not a click aims at.
    Row,
    /// The tree pane, past its last row.
    Pane,
    /// The help overlay.
    Help,
    /// One of a confirmation's two answers.
    Answer,
    /// A confirmation, off both of its answers.
    Question,
    /// The filter prompt.
    Prompt,
    /// Outside whichever overlay is up.
    Away,
    /// Chrome, or a frame with nothing on it.
    Elsewhere,
}

impl Target {
    /// Every target there is, for the assertions over the table.
    pub const ALL: [Self; 12] = [
        Self::Heading,
        Self::Box,
        Self::Indicator,
        Self::Name,
        Self::Row,
        Self::Pane,
        Self::Help,
        Self::Answer,
        Self::Question,
        Self::Prompt,
        Self::Away,
        Self::Elsewhere,
    ];

    /// What this gesture, landing here, is aimed at.
    ///
    /// The zones are a **click**'s business and nothing else's. A double click and a wheel
    /// turn are both claims about the row rather than about the cell of it under the hand —
    /// "double-click a row" is what the gesture means everywhere, and a wheel that scrolled
    /// differently over the mark box would be unusable.
    fn of(gesture: Gesture, spot: Spot) -> Self {
        match spot {
            Spot::Heading(_) => Self::Heading,
            Spot::Row { zone, .. } if matches!(gesture, Gesture::Click) => match zone {
                Zone::Mark => Self::Box,
                Zone::Open => Self::Indicator,
                Zone::Name => Self::Name,
            },
            Spot::Row { .. } => Self::Row,
            Spot::Tree => Self::Pane,
            Spot::Help => Self::Help,
            Spot::Answer(_) => Self::Answer,
            Spot::Confirm => Self::Question,
            Spot::Prompt => Self::Prompt,
            Spot::Outside => Self::Away,
            Spot::Nowhere => Self::Elsewhere,
        }
    }
}

/// One row of the pointer map: a gesture, where it lands, and what it does there.
#[derive(Clone, Debug)]
pub struct Pointing {
    /// Which gesture.
    pub gesture: Gesture,
    /// Every spot it means this on. Several, exactly as a [`Binding`] has several chords: the
    /// wheel scrolls the tree over the heading, over a row and over the empty pane below.
    pub targets: &'static [Target],
    /// How the help overlay names where it lands, as the object of [`Gesture`]'s verb.
    pub place: &'static str,
    /// The sentence the help overlay prints, in the same lower-case imperative the keymap's
    /// sentences use.
    pub what: &'static str,
    /// What it produces. A function of the spot, because the action carries the identity the
    /// table deliberately does not.
    deed: fn(Gesture, Spot) -> Action,
}

impl Pointing {
    /// The gesture, spelled as the overlay spells it.
    #[must_use]
    pub fn how(&self) -> String {
        format!("{} {}", self.gesture, self.place)
    }
}

const fn point(
    gesture: Gesture,
    targets: &'static [Target],
    place: &'static str,
    what: &'static str,
    deed: fn(Gesture, Spot) -> Action,
) -> Pointing {
    Pointing {
        gesture,
        targets,
        place,
        what,
        deed,
    }
}

/// Every pointer gesture pristine has, in help order.
///
/// # Only the left button, and the wheel
///
/// The right and middle are left to the terminal's own menus. A **drag** is deliberately
/// absent from this table and that is not an omission: a press that moved is not a click, and
/// there is nothing else for it to be here — pristine has no text selection of its own, so a
/// drag's only job is to make sure the press it belongs to never becomes one. See
/// [`super::Pointer`].
static POINTER: LazyLock<Vec<Pointing>> = LazyLock::new(|| {
    vec![
        point(
            Gesture::Click,
            &[Target::Heading],
            "a column heading",
            "order the levels by it — again to turn it upside down",
            sort_by,
        ),
        point(
            Gesture::Click,
            &[Target::Box],
            "a row's box",
            "mark this row's whole subtree, or unmark it",
            mark_row,
        ),
        point(
            Gesture::Click,
            &[Target::Indicator],
            "a row's ▸",
            "open the row, or close it",
            open_row,
        ),
        point(
            Gesture::Click,
            &[Target::Name],
            "a row's name",
            "put the cursor on it",
            select,
        ),
        point(
            Gesture::Double,
            &[Target::Row],
            "a row",
            "price this subtree — what --breakdown-under does, on one directory",
            price,
        ),
        point(
            Gesture::Aim,
            &[Target::Answer],
            "a confirmation's answer",
            "highlight it, so the button under the pointer is the one a click takes",
            aim,
        ),
        point(
            Gesture::Click,
            &[Target::Answer],
            "a confirmation's answer",
            "answer with it — the press has to have landed on it too",
            answer,
        ),
        point(
            Gesture::Click,
            &[Target::Away],
            "outside an overlay",
            "close it, exactly as Esc does",
            dismiss,
        ),
        point(
            Gesture::Wheel(Motion::Down),
            &[Target::Heading, Target::Row, Target::Pane],
            "the tree",
            "scroll the rows",
            scroll_rows,
        ),
        point(
            Gesture::Wheel(Motion::Down),
            &[Target::Help],
            "the help",
            "scroll the page",
            scroll_page,
        ),
    ]
});

/// The whole pointer map, for anything that renders or checks it.
#[must_use]
pub fn pointing() -> &'static [Pointing] {
    &POINTER
}

/// What one mouse gesture means, given what it landed on.
///
/// There is no chain here, and that is the point. A key is offered to a list of surfaces
/// because a keyboard has no coordinates; a press has them, so the same question is answered
/// by [`super::render::hit`] — an overlay covers what it is over, so a press inside one cannot
/// also be a press on the tree, and one outside is the dismissal.
#[must_use]
pub fn pointer(gesture: Gesture, spot: Spot) -> Action {
    let target = Target::of(gesture, spot);
    pointing()
        .iter()
        .find(|row| row.gesture.same(gesture) && row.targets.contains(&target))
        .map_or(Action::Ignore, |row| (row.deed)(gesture, spot))
}

/// Letting go of a press that never moved — the click.
///
/// It acts on what the **press** landed on rather than on what the release did, which is the
/// only rule that can be right: the press is the aimed half of the gesture, resolved against
/// the frame the reader was looking at when they aimed.
///
/// A confirmation is the exception, and it is the one surface where the worst case is
/// irreversible: there both halves have to land in the same button. That single equality is
/// the whole guard, and it is stronger than it looks — [`Spot::Answer`] exists only on a frame
/// that drew a question, so a press made *before* the box appeared can never equal one, and a
/// box arriving under a held button cannot be answered by the hand that was already down.
#[must_use]
pub fn finish(pressed: Spot, double: bool, released: Spot) -> Action {
    if matches!(pressed, Spot::Answer(_)) && pressed != released {
        return Action::Ignore;
    }
    pointer(
        if double {
            Gesture::Double
        } else {
            Gesture::Click
        },
        pressed,
    )
}

/// A click on a row, with the identity the table left out put back.
fn on_row(spot: Spot, deed: fn(crate::tree::NodeId) -> Action) -> Action {
    match spot {
        Spot::Row { id, .. } => deed(id),
        _ => Action::Ignore,
    }
}

fn select(_: Gesture, spot: Spot) -> Action {
    on_row(spot, Action::Select)
}

fn open_row(_: Gesture, spot: Spot) -> Action {
    on_row(spot, Action::OpenRow)
}

fn mark_row(_: Gesture, spot: Spot) -> Action {
    on_row(spot, Action::MarkRow)
}

fn price(_: Gesture, spot: Spot) -> Action {
    on_row(spot, Action::Price)
}

fn sort_by(_: Gesture, spot: Spot) -> Action {
    match spot {
        Spot::Heading(order) => Action::SortBy(order),
        _ => Action::Ignore,
    }
}

/// Aiming at an answer moves the *keyboard's* highlight, which is what keeps the pointer and
/// the arrow keys driving one selection rather than two.
fn aim(_: Gesture, spot: Spot) -> Action {
    match spot {
        Spot::Answer(answer) => Action::Highlight(answer.turn()),
        _ => Action::Ignore,
    }
}

/// Taking the answer the press aimed at, which the press has already highlighted.
fn answer(_: Gesture, spot: Spot) -> Action {
    match spot {
        Spot::Answer(_) => Action::Answer,
        _ => Action::Ignore,
    }
}

fn dismiss(_: Gesture, _: Spot) -> Action {
    Action::Back
}

fn scroll_rows(gesture: Gesture, _: Spot) -> Action {
    gesture.motion().map_or(Action::Ignore, Action::ScrollRows)
}

fn scroll_page(gesture: Gesture, _: Spot) -> Action {
    gesture.motion().map_or(Action::Ignore, Action::Scroll)
}

/// The help page, as headed groups of `(keys, sentence)`.
///
/// Generated from the tables rather than written, which is what keeps it honest: a binding
/// added without a sentence does not compile, and a sentence with no binding cannot exist.
/// The pointer's rows are generated the same way and for the same reason — a gesture that
/// acts while being undocumented is exactly the failure these tables exist to make
/// impossible.
#[must_use]
pub fn help() -> Vec<(&'static str, Vec<(String, &'static str)>)> {
    let surfaces = [
        Surface::Global,
        Surface::Tree,
        Surface::Prompt,
        Surface::Confirm,
        Surface::Help,
    ];
    let mut page: Vec<(&'static str, Vec<(String, &'static str)>)> = surfaces
        .into_iter()
        .map(|surface| {
            let rows = bindings()
                .iter()
                .filter(|binding| binding.surface == surface)
                .map(|binding| (binding.keys(), binding.what))
                .collect();
            (surface.title(), rows)
        })
        .collect();
    page.push((
        "The pointer",
        pointing().iter().map(|row| (row.how(), row.what)).collect(),
    ));
    page
}

#[cfg(test)]
mod tests {
    use super::{
        Action, Chord, Gesture, Motion, Overlay, Spot, Surface, Target, Turn, Zone, action_for,
        bindings, finish, help, lookup, pointer, pointing,
    };
    use crate::tree::{NodeId, Order};
    use crate::tui::state::Answer;
    use ratatui::crossterm::event::{
        Event, KeyCode, KeyEvent, KeyEventKind, KeyEventState, KeyModifiers,
    };

    fn press(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }

    fn letter(letter: char) -> Event {
        press(KeyCode::Char(letter))
    }

    /// A spot of each kind, for the assertions that sweep the whole pointer map.
    ///
    /// One per [`Target`], in the same order, so a target added without a spot to reach it
    /// with does not compile.
    fn spot(target: Target) -> Spot {
        const ROW: NodeId = 7;
        match target {
            Target::Heading => Spot::Heading(Order::Size),
            Target::Box => Spot::Row {
                id: ROW,
                zone: Zone::Mark,
            },
            Target::Indicator => Spot::Row {
                id: ROW,
                zone: Zone::Open,
            },
            // A row aimed at by something other than a click resolves to `Target::Row`
            // whichever zone it names, so the name is as good a stand-in as any.
            Target::Name | Target::Row => Spot::Row {
                id: ROW,
                zone: Zone::Name,
            },
            Target::Pane => Spot::Tree,
            Target::Help => Spot::Help,
            Target::Answer => Spot::Answer(Answer::Delete),
            Target::Question => Spot::Confirm,
            Target::Prompt => Spot::Prompt,
            Target::Away => Spot::Outside,
            Target::Elsewhere => Spot::Nowhere,
        }
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
    fn the_help_page_lists_every_binding_and_every_gesture_there_is() {
        let listed: usize = help().iter().map(|(_, rows)| rows.len()).sum();
        assert_eq!(listed, bindings().len() + pointing().len());
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

    // ---- the pointer ------------------------------------------------------------------

    #[test]
    fn no_row_of_the_pointer_map_is_dead() {
        // The table *is* the dispatcher, so "a gesture that acts is a gesture the help page
        // lists" holds by construction. What does not is the other direction: a row wired to
        // the wrong shape of spot — `sort_by` under a `Row` target — would document a
        // gesture that quietly does nothing. Each row is asked for its own gesture on each
        // of its own targets, which is exactly the sentence the help page prints.
        for row in pointing() {
            for &target in row.targets {
                assert_ne!(
                    pointer(row.gesture, spot(target)),
                    Action::Ignore,
                    "{:?} does nothing, and the help page says {:?}",
                    row.how(),
                    row.what
                );
            }
        }
    }

    #[test]
    fn no_spot_gives_one_gesture_two_meanings() {
        for gesture in [
            Gesture::Aim,
            Gesture::Click,
            Gesture::Double,
            Gesture::Wheel(Motion::Down),
        ] {
            for target in Target::ALL {
                let claimants = pointing()
                    .iter()
                    .filter(|row| row.gesture.same(gesture) && row.targets.contains(&target))
                    .count();
                assert!(claimants <= 1, "{gesture} {target:?} is bound twice");
            }
        }
    }

    #[test]
    fn a_row_is_named_by_its_directory_and_never_by_where_it_is_on_the_screen() {
        // The rule the whole model turns on. The action a press produces carries the
        // `NodeId`, so the row it acts on is the one that was pressed even after a price
        // lands and re-sorts the level under the hand.
        let spot = Spot::Row {
            id: 42,
            zone: Zone::Name,
        };
        assert_eq!(pointer(Gesture::Click, spot), Action::Select(42));
        assert_eq!(pointer(Gesture::Double, spot), Action::Price(42));
    }

    #[test]
    fn the_zones_of_a_row_are_a_clicks_business_and_nothing_elses() {
        // Each part of a row does its own thing under a click…
        for (zone, action) in [
            (Zone::Mark, Action::MarkRow(3)),
            (Zone::Open, Action::OpenRow(3)),
            (Zone::Name, Action::Select(3)),
        ] {
            assert_eq!(
                pointer(Gesture::Click, Spot::Row { id: 3, zone }),
                action,
                "{zone:?}"
            );
            // …and every part of it is the same row to a double click and to the wheel. A
            // wheel that scrolled differently over the mark box would be unusable, and
            // "double-click a row" is what the gesture means everywhere.
            assert_eq!(
                pointer(Gesture::Double, Spot::Row { id: 3, zone }),
                Action::Price(3),
                "{zone:?}"
            );
            assert_eq!(
                pointer(Gesture::Wheel(Motion::Down), Spot::Row { id: 3, zone }),
                Action::ScrollRows(Motion::Down),
                "{zone:?}"
            );
        }
    }

    #[test]
    fn a_press_aims_and_does_no_more_than_aim() {
        // The rule the deferred click exists for: at the moment the button goes down there
        // is no telling a click from a drag, so a press that acted would re-sort the tree
        // under the hand that was about to select from it.
        for target in Target::ALL {
            let aimed = pointer(Gesture::Aim, spot(target));
            let expected = match target {
                // The one exception, and it is not really one: highlighting the button under
                // the pointer moves a selection, which a hover would have moved too.
                Target::Answer => Action::Highlight(Turn::Next),
                _ => Action::Ignore,
            };
            assert_eq!(aimed, expected, "{target:?}");
        }
    }

    #[test]
    fn the_click_acts_on_what_the_press_was_aimed_at() {
        // The release's own spot is not the target: a hand that lets go a cell off the
        // heading it pressed has still clicked that heading, and the press is the aimed half
        // of the gesture.
        assert_eq!(
            finish(Spot::Heading(Order::Age), false, Spot::Nowhere),
            Action::SortBy(Order::Age)
        );
    }

    #[test]
    fn a_confirmation_is_the_one_surface_that_needs_both_halves_in_the_same_button() {
        let delete = Spot::Answer(Answer::Delete);
        assert_eq!(finish(delete, false, delete), Action::Answer);

        // Landing near an answer, or on the other one, is a miss — and so is a press made
        // *before* the box appeared, which is the case that matters: `Spot::Answer` exists
        // only on a frame that drew a question, so such a press can never equal one, and a
        // box arriving under a held button cannot be answered by the hand already down.
        assert_eq!(finish(delete, false, Spot::Confirm), Action::Ignore);
        assert_eq!(
            finish(delete, false, Spot::Answer(Answer::Cancel)),
            Action::Ignore
        );
        assert_eq!(finish(Spot::Tree, false, delete), Action::Ignore);
    }

    #[test]
    fn a_press_inside_an_overlay_that_lands_on_nothing_does_nothing() {
        // Deliberately not a dismissal: a press on a caveat line or on the prompt's own text
        // is a miss, and closing the thing being read is the one response that loses work.
        for spot in [Spot::Help, Spot::Prompt, Spot::Confirm, Spot::Nowhere] {
            assert_eq!(pointer(Gesture::Click, spot), Action::Ignore, "{spot:?}");
        }
        // Outside it is the dismissal, which is `Esc`'s own action rather than a second one.
        assert_eq!(pointer(Gesture::Click, Spot::Outside), Action::Back);
    }

    #[test]
    fn the_wheel_means_the_nearest_thing_to_scrolling_each_surface_has() {
        // Over the tree it moves the viewport; over the help overlay it moves a document.
        // Those are genuinely different verbs, and over a question it is neither — a wheel
        // is not a way past something that swallowed the keyboard.
        assert_eq!(
            pointer(Gesture::Wheel(Motion::Up), Spot::Tree),
            Action::ScrollRows(Motion::Up)
        );
        assert_eq!(
            pointer(Gesture::Wheel(Motion::Down), Spot::Help),
            Action::Scroll(Motion::Down)
        );
        for spot in [Spot::Confirm, Spot::Answer(Answer::Delete), Spot::Nowhere] {
            assert_eq!(
                pointer(Gesture::Wheel(Motion::Down), spot),
                Action::Ignore,
                "{spot:?}"
            );
        }
    }

    #[test]
    fn the_help_page_spells_a_gesture_the_way_a_reader_would_say_it() {
        let page = help();
        let (title, rows) = page.last().unwrap();
        assert_eq!(*title, "The pointer");
        let said: Vec<&str> = rows.iter().map(|(how, _)| how.as_str()).collect();
        assert!(said.contains(&"double-click a row"), "{said:?}");
        assert!(said.contains(&"wheel over the tree"), "{said:?}");
        assert!(said.contains(&"click a column heading"), "{said:?}");
    }
}
