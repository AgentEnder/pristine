//! Drawing one frame.
//!
//! Everything here reads the [`View`] and writes cells. The one thing it writes *back* is a
//! measurement — how many rows the tree pane got, and how far the help page can scroll —
//! because those are facts about the frame that nothing else knows.
//!
//! # What a row has to say
//!
//! Four things, in the order a reader needs them: how much of it is marked, where it is,
//! what it is worth, and when it was last touched. The last two are npkill's columns and the
//! first is what npkill's flat list cannot have.
//!
//! The size column has a fifth state the other tools never needed: **unpriced**. A claim is
//! recorded without being measured unless somebody asks, so `0 B` and "nobody has looked" are
//! different facts about a row, and a dash is what keeps them apart. It has a sixth as well,
//! which is the one that moves: a claim a pricing thread is *inside at this instant* draws a
//! shimmer through that dash rather than the dash. The pool is bounded, so however many rows
//! shimmer is however many threads are working, and that is a number worth being able to see.
//!
//! # Nothing here decides what moves, only how it looks
//!
//! Every animated value is asked for by name — [`View::drawn`], [`View::freshness`],
//! [`View::is_pricing`] — and the view has already advanced them all once for this frame. So
//! this file has no clock in it and no state between frames, and the drawing stays a pure
//! function of the view, which is what keeps it assertable against a [`TestBackend`] one
//! property at a time.
//!
//! [`TestBackend`]: ratatui::backend::TestBackend

use ratatui::Frame;
use ratatui::layout::{Alignment, Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, Wrap};

use super::keymap::help;
use super::state::{Mark, Roll, View, plural};
use crate::size::human;
use crate::tree::NodeId;
use crate::walk::WalkError;

/// Bytes the size column is given. Exactly `> 1023.9 GiB`, which is the widest a rolled-up
/// lower bound gets.
const SIZE: u16 = 12;
/// Cells for `3 months`.
const AGE: u16 = 9;
/// Cells for the regeneration command, or for the reason a removal left a directory standing.
const REGENERATE: u16 = 30;
/// Below this the last column is dropped: a path a reader cannot read is worse than a command
/// they have to press `Enter` on the row to see.
const NARROW: u16 = 94;
/// How many cells the pricing shimmer travels across, in place of the dash.
const SHIMMER: usize = 5;
/// The eight steps a partial mark is filled to, one per eighth.
///
/// Never empty and never full: an empty box is [`Mark::None`] and a full one is [`Mark::All`],
/// so a partial row is always somewhere strictly in between and the glyph says where.
const BLOCKS: [&str; 7] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇"];

/// Draws the whole frame.
pub fn draw(frame: &mut Frame, view: &mut View, errors: &[WalkError]) {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(headline(view, errors), header);
    view.viewport(body.height as usize);
    frame.render_widget(tree(view, body.width, body.height as usize), body);
    frame.render_widget(status(view), footer);

    if let Some(prompt) = view.prompt() {
        let line = Line::from(vec![
            Span::styled("/", Style::default().fg(Color::Yellow)),
            Span::raw(prompt.text()),
            match prompt.error() {
                Some(err) => Span::styled(
                    format!("   {err}"),
                    Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
                ),
                None => Span::raw(""),
            },
        ]);
        frame.render_widget(Paragraph::new(line), footer);
        // The caret is the terminal's own, so a reader's cursor is where they are typing
        // rather than drawn as a block that their terminal's blink rate disagrees with.
        frame.set_cursor_position((
            footer.x + 1 + u16::try_from(prompt.caret()).unwrap_or(u16::MAX),
            footer.y,
        ));
    }

    if let Some(pending) = view.pending() {
        let mut lines = vec![
            Line::from(format!(
                "Delete {}, giving back {}?",
                plural(pending.targets.len(), "directory", "directories"),
                human(pending.bytes)
            )),
            Line::raw(""),
        ];
        if pending.unpriced > 0 {
            lines.push(Line::styled(
                format!(
                    "{} of them carry no price yet, so the figure is a floor.",
                    pending.unpriced
                ),
                Style::default().fg(Color::DarkGray),
            ));
        }
        for kept in pending.kept.iter().take(4) {
            lines.push(Line::styled(
                format!("left alone — {kept}"),
                Style::default().fg(Color::Yellow),
            ));
        }
        if pending.kept.len() > 4 {
            lines.push(Line::styled(
                format!("…and {} more left alone", pending.kept.len() - 4),
                Style::default().fg(Color::Yellow),
            ));
        }
        lines.push(Line::raw(""));
        lines.push(Line::from(vec![
            answer("cancel", pending.answer == super::state::Answer::Cancel),
            Span::raw("   "),
            answer("delete", pending.answer == super::state::Answer::Delete),
        ]));
        let area = centred(
            frame.area(),
            66,
            u16::try_from(lines.len()).unwrap_or(8) + 2,
        );
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(lines)
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" this cannot be undone ")
                        .border_style(Style::default().fg(Color::Red)),
                )
                .wrap(Wrap { trim: true }),
            area,
        );
    }

    if let Some(at) = view.help() {
        let area = centred(frame.area(), 74, frame.area().height.saturating_sub(4));
        let page = help_page();
        let height = area.height.saturating_sub(2) as usize;
        view.clamp_help(page.lines.len().saturating_sub(height));
        let at = view.help().unwrap_or(at);
        frame.render_widget(Clear, area);
        frame.render_widget(
            Paragraph::new(page)
                .scroll((u16::try_from(at).unwrap_or(u16::MAX), 0))
                .block(
                    Block::default()
                        .borders(Borders::ALL)
                        .title(" keys — Esc or ? to close "),
                ),
            area,
        );
    }
}

/// The line across the top: where the scan is, what it has found, and whether it is done.
fn headline(view: &View, errors: &[WalkError]) -> Paragraph<'static> {
    let total = view.drawn_total();
    let mut spans = vec![
        Span::styled(
            format!(" {} ", view.tree().root_path().display()),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        // The drawn total rather than the true one, so it climbs as the scan finds and falls
        // as a removal frees. Which of the two it is doing is the one thing this line cannot
        // say in words and can say by moving.
        Span::raw(format!(
            " {} reclaimable in {} ",
            total.label(),
            plural(total.claims, "directory", "directories")
        )),
    ];
    if total.unpriced > 0 {
        // Two different facts, and saying the wrong one is a small lie a reader would catch:
        // while the walk is running an unpriced claim is one the pool has not reached yet,
        // and afterwards — under `--breakdown-under` — it is one nobody is ever going to
        // price.
        spans.push(Span::styled(
            if view.is_scanning() {
                format!("· {} still being priced ", total.unpriced)
            } else {
                format!("· {} unpriced ", total.unpriced)
            },
            Style::default().fg(Color::DarkGray),
        ));
    }
    if view.is_scanning() {
        spans.push(Span::styled(
            "· scanning ",
            Style::default().fg(Color::Cyan),
        ));
    }
    if let Some(pattern) = view.filter() {
        spans.push(Span::styled(
            format!("· /{pattern} "),
            Style::default().fg(Color::Yellow),
        ));
    }
    if !errors.is_empty() {
        // The listing's rule, kept: a scan that could not read everything says so beside the
        // numbers it qualifies, because a lower bound that looks like a total is the one
        // wrong answer a cleaner must not give.
        spans.push(Span::styled(
            format!(
                "· {} unread, so this is a floor ",
                plural(errors.len(), "path", "paths")
            ),
            Style::default().fg(Color::Red),
        ));
    }
    Paragraph::new(Line::from(spans)).style(Style::default().bg(Color::Rgb(32, 32, 40)))
}

/// The tree itself: one table row per *visible* row, from the scroll offset down.
///
/// Bounded by the pane rather than by the tree, which is the difference between drawing a screen
/// and building one. A home directory fully expanded is 32,634 rows; the widget would draw the
/// first `height` of them either way, but every row handed to it is a `Vec` of styled spans
/// allocated first and thrown away second.
fn tree(view: &View, width: u16, height: usize) -> Table<'static> {
    let wide = width >= NARROW;
    let rows: Vec<TableRow> = view
        .rows()
        .iter()
        .enumerate()
        .skip(view.scroll())
        .take(height)
        .map(|(at, row)| {
            let node = view.tree().node(row.id);
            let selected = view.cursor() == Some(at);
            let mut cells = vec![
                Cell::from(Line::from(label(view, row.id, row.depth))),
                Cell::from(size_of(view, row.id)),
                Cell::from(Text::from(age(node.modified)).alignment(Alignment::Right)),
            ];
            if wide {
                cells.push(aside(view, row.id));
            }
            let style = if selected {
                Style::default()
                    .bg(Color::Rgb(48, 48, 64))
                    .add_modifier(Modifier::BOLD)
            } else if view.is_spent(row.id) {
                // Emptied, and on its way out — dimmed only once its number has reached zero,
                // never while it is still falling. A row dimmed throughout would be saying
                // "this is over" during the seconds it is actually happening, which is the
                // opposite of what the falling number is for. Dim rather than struck through
                // or coloured: the directory is gone and the row is a receipt now, so it
                // should recede rather than compete.
                Style::default().fg(Color::DarkGray)
            } else {
                arrival(view, row.id)
            };
            TableRow::new(cells).style(style)
        })
        .collect();

    let mut widths = vec![
        Constraint::Min(20),
        Constraint::Length(SIZE),
        Constraint::Length(AGE),
    ];
    if wide {
        widths.push(Constraint::Length(REGENERATE));
    }
    Table::new(rows, widths).column_spacing(2)
}

/// A row's marker, indent, expander and name, as one run of spans.
fn label(view: &View, id: NodeId, depth: usize) -> Vec<Span<'static>> {
    let node = view.tree().node(id);
    let name = if node.parent.is_none() {
        node.path.display().to_string()
    } else {
        node.name.to_string_lossy().into_owned()
    };
    vec![
        marker(view, id),
        Span::raw("  ".repeat(depth)),
        Span::raw(if view.tree().children(id).is_empty() {
            "  "
        } else if view.is_expanded(id) {
            "▾ "
        } else {
            "▸ "
        }),
        Span::styled(
            name,
            if view.kept_reason(id).is_some() {
                // A directory the safety model refused. Distinct, and deliberately not red:
                // this is the tool working, and teaching a reader to read correct behaviour
                // as a failure is worse than not marking it at all.
                Style::default().fg(Color::Cyan)
            } else if node.hit.is_some() {
                Style::default().fg(Color::White)
            } else {
                Style::default().fg(Color::Rgb(150, 160, 180))
            },
        ),
    ]
}

/// The mark box: empty, full, or a block filled to the marked share of the subtree.
///
/// The fractional block is what makes a partial ancestor worth reading rather than merely
/// noticing. `[~]` says "some of this"; `[▆]` says most of the bytes under here are spoken
/// for, which is a real number in one character and the thing a reader needs in order to
/// decide whether opening the row is worth it.
fn marker(view: &View, id: NodeId) -> Span<'static> {
    let (glyph, colour) = match view.mark_of(id) {
        Mark::None => ("[ ] ".to_owned(), Color::DarkGray),
        Mark::Partial => {
            #[expect(
                clippy::cast_precision_loss,
                clippy::cast_possible_truncation,
                clippy::cast_sign_loss,
                reason = "a share in 0..1 scaled to one of seven glyphs, clamped either side"
            )]
            let step = (view.share(id) * BLOCKS.len() as f64)
                .round()
                .clamp(1.0, 7.0) as usize;
            (format!("[{}] ", BLOCKS[step - 1]), Color::Yellow)
        }
        Mark::All => ("[x] ".to_owned(), Color::Green),
    };
    let style = if view.is_cascading(id) {
        // The mark passing through on its way up. Reversed rather than merely brighter,
        // because it is on screen for a sixth of a second and has to be caught rather than
        // studied.
        Style::default()
            .fg(Color::Black)
            .bg(Color::Green)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(colour)
    };
    Span::styled(glyph, style)
}

/// The size column, which is where most of the movement is.
///
/// Three states rather than the two a listing has. A claim a pricing thread is inside right
/// now gets the shimmer; everything else gets its number, climbing toward the truth, with a
/// `>` in front of it while any of what it is summing is still unpriced.
fn size_of(view: &View, id: NodeId) -> Text<'static> {
    if view.is_pricing(id) {
        let lit = view.shimmer(SHIMMER);
        return Text::from(Line::from(
            (0..SHIMMER)
                .map(|cell| {
                    if cell == lit {
                        Span::styled(
                            "━",
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD),
                        )
                    } else {
                        Span::styled("─", Style::default().fg(Color::Rgb(70, 78, 92)))
                    }
                })
                .collect::<Vec<_>>(),
        ))
        .alignment(Alignment::Right);
    }
    let roll = view.drawn(id);
    let style = if roll.unpriced > 0 && roll.bytes > 0 {
        // A floor is a different kind of number from a total, and it reads as one.
        Style::default().fg(Color::Rgb(150, 160, 180))
    } else {
        Style::default()
    };
    Text::from(Line::styled(roll.label(), style)).alignment(Alignment::Right)
}

/// The last column: what brings this row back, or why a removal left it standing.
///
/// The refusal wins, because it is the newer and the more surprising fact. A reader who marked
/// forty directories and got thirty-eight needs to see which two on the rows themselves — the
/// footer has one line and it has already moved on to the next thing.
fn aside(view: &View, id: NodeId) -> Cell<'static> {
    match view.kept_reason(id) {
        Some(reason) => Cell::from(Span::styled(
            format!("kept — {reason}"),
            Style::default().fg(Color::Cyan),
        )),
        None => Cell::from(Span::styled(
            regenerate(view, id),
            Style::default().fg(Color::DarkGray),
        )),
    }
}

/// How lit a row is because the walk has just found it.
///
/// A decaying wash rather than a permanent colour, and it is the alternative to a scrolling
/// log: the eye is drawn to what is new without the tree having to give up being a tree. It
/// fades on a square root rather than linearly, so it holds long enough to be *looked at*
/// after it is noticed instead of already going by the time the eye lands.
fn arrival(view: &View, id: NodeId) -> Style {
    let lit = view.freshness(id);
    if lit <= 0.0 {
        return Style::default();
    }
    let lit = lit.sqrt();
    #[expect(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "a channel scaled by a factor between zero and one, clamped by construction"
    )]
    let channel = |peak: f64| (peak * lit).round() as u8;
    Style::default().bg(Color::Rgb(channel(26.0), channel(62.0), channel(44.0)))
}

/// What brings this row back, when anything knows.
///
/// Only on a claim: an ancestor covers several rules at once, and a directory holding a
/// `node_modules` and a `target` has no single command that rebuilds it. The tier-two gap is
/// carried through rather than papered over — "no known way" is the information that the
/// deletion is not a cheap one.
fn regenerate(view: &View, id: NodeId) -> String {
    match &view.tree().node(id).hit {
        Some(hit) => hit.regenerate().unwrap_or("no known way back").to_owned(),
        None => String::new(),
    }
}

/// How long ago, in the coarsest unit that is still true.
fn age(modified: Option<std::time::SystemTime>) -> String {
    let Some(modified) = modified else {
        return crate::size::UNPRICED.to_owned();
    };
    let Ok(since) = std::time::SystemTime::now().duration_since(modified) else {
        // A directory stamped in the future: a clock that has been put back, or a filesystem
        // that never had one. "now" is the honest reading and it is also the safe one, since
        // an age floor is a reason to keep something.
        return "now".to_owned();
    };
    let days = since.as_secs() / 86_400;
    match days {
        0 => format!("{}h", since.as_secs() / 3_600),
        1..=30 => format!("{days}d"),
        31..=364 => format!("{}mo", days / 30),
        _ => format!("{}y", days / 365),
    }
}

/// The line across the bottom: what is marked, or what just happened.
fn status(view: &View) -> Paragraph<'static> {
    // The freed counter outlives the notice, because it is the answer to the question the
    // reader who walked away came back for. It is also the *other* of the two numbers a
    // removal moves — the header falls, this rises — and the pair of them is the whole payoff
    // of the one irreversible thing this tool does. Nothing decorates it.
    let freed = view.has_freed().then(|| {
        Span::styled(
            format!("· freed {} ", human(view.drawn_freed())),
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD),
        )
    });
    let said = Style::default().fg(Color::Black).bg(Color::Yellow);
    // Where the deleter is, which the freed counter beside it cannot say: bytes report how
    // much has gone and nothing about how much is left, so a count against the batch's own
    // size is what tells a third of the way through from nearly finished.
    //
    // **Beside a notice rather than instead of one.** The only thing that speaks while a
    // removal is running is `q`, which is held back until the batch finishes and says so — and
    // a reader who pressed a key and got nothing back has no way to tell a refusal from a
    // terminal that stopped listening.
    let mut spans: Vec<Span<'static>> = Vec::new();
    if let Some(removing) = view.removing() {
        spans.push(Span::styled(format!(" {} ", removing.label()), said));
    }
    if let Some(notice) = view.notice() {
        spans.push(Span::styled(format!(" {notice} "), said));
    }
    if !spans.is_empty() {
        spans.extend(freed);
        return Paragraph::new(Line::from(spans));
    }
    let marked = view.marked();
    let mut spans = vec![Span::styled(
        format!(" {} ", counter(marked)),
        if marked.claims == 0 {
            Style::default().fg(Color::DarkGray)
        } else {
            Style::default()
                .fg(Color::Green)
                .add_modifier(Modifier::BOLD)
        },
    )];
    spans.extend(freed);
    if view.is_deleting() {
        spans.push(Span::styled(
            "· deleting ",
            Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
        ));
    }
    spans.push(Span::styled(
        format!(
            "· space mark · x delete · / filter · s sort ({}{}) · ? help",
            view.sort().by.label(),
            if view.sort().reverse { " ↑" } else { "" }
        ),
        Style::default().fg(Color::DarkGray),
    ));
    Paragraph::new(Line::from(spans))
}

/// npkill's selection counter, over subtrees rather than rows.
fn counter(marked: Roll) -> String {
    if marked.claims == 0 {
        return "nothing marked".to_owned();
    }
    let said = format!(
        "marked {} in {}",
        human(marked.bytes),
        plural(marked.claims, "directory", "directories")
    );
    if marked.unpriced > 0 {
        // The count rather than a bigger number, because there is no bigger number to give:
        // an unpriced claim's bytes are not a small contribution, they are an unknown one.
        return format!("{said} (+{} unpriced)", marked.unpriced);
    }
    said
}

/// One of a confirmation's two answers, highlighted or not.
fn answer(name: &'static str, highlighted: bool) -> Span<'static> {
    let style = if highlighted {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };
    Span::styled(format!("  {name}  "), style)
}

/// The help page, generated from the keymap so it cannot drift from what the keys do.
fn help_page() -> Text<'static> {
    let mut lines = Vec::new();
    for (title, rows) in help() {
        if !lines.is_empty() {
            lines.push(Line::raw(""));
        }
        lines.push(Line::styled(
            title,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ));
        for (keys, what) in rows {
            lines.push(Line::from(vec![
                Span::styled(format!("  {keys:<18}"), Style::default().fg(Color::Yellow)),
                Span::raw(what),
            ]));
        }
    }
    Text::from(lines)
}

/// A box of this size in the middle of `area`, clamped to fit.
fn centred(area: Rect, width: u16, height: u16) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + (area.width - width) / 2,
        y: area.y + (area.height - height) / 2,
        width,
        height,
    }
}

#[cfg(test)]
mod tests {
    use super::draw;
    use crate::delete::{Refusal, Refused};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::Tree;
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::moving::COUNT_UP;
    use crate::tui::state::{Answer, Pending, View};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use std::path::Path;

    /// Opens every row. Twice, because the root starts open and the first `*` closes it.
    fn open_everything(view: &mut View) {
        view.apply(Action::Cursor(Motion::Top));
        view.apply(Action::ToggleSubtree);
        view.apply(Action::ToggleSubtree);
    }

    /// The one drawn line holding `needle`.
    fn row_with<'a>(frame: &'a [String], needle: &str) -> &'a str {
        frame
            .iter()
            .find(|line| line.contains(needle))
            .unwrap_or_else(|| panic!("no row mentions {needle}: {frame:#?}"))
    }

    /// Everything the frame drew, one line per row, trailing blanks trimmed.
    fn painted(view: &mut View, width: u16, height: u16) -> Vec<String> {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        terminal.draw(|frame| draw(frame, view, &[])).unwrap();
        let buffer = terminal.backend().buffer().clone();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect()
    }

    fn view() -> View {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/node_modules", 2 * 1024 * 1024));
        tree.insert(hit("/scan/old/target", Size::Unmeasured, 0));
        View::new(tree)
    }

    #[test]
    fn a_row_carries_its_marker_its_rollup_and_what_brings_it_back() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        let frame = painted(&mut view, 100, 8);

        assert!(frame[0].contains("/scan"), "{frame:#?}");
        assert!(
            frame[0].contains("2.0 MiB reclaimable in 2 directories"),
            "{frame:#?}"
        );
        // The marked row, its rolled-up size, and — because the terminal is wide enough —
        // the command that brings it back.
        let marked = frame.iter().find(|line| line.contains("nx")).unwrap();
        assert!(marked.contains("[x]"), "{marked}");
        assert!(marked.contains("▸"), "{marked}");
        assert!(marked.contains("2.0 MiB"), "{marked}");
        // …and the row nobody has priced shows a dash rather than a zero.
        let unpriced = frame.iter().find(|line| line.contains("old")).unwrap();
        assert!(unpriced.contains("—"), "{unpriced}");
        assert!(!unpriced.contains("0 B"), "{unpriced}");
    }

    #[test]
    fn the_footer_counts_what_is_marked() {
        let mut view = view();
        let frame = painted(&mut view, 100, 8);
        assert!(frame[7].contains("nothing marked"), "{frame:#?}");

        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        let frame = painted(&mut view, 100, 8);
        assert!(
            frame[7].contains("marked 2.0 MiB in 1 directory"),
            "{frame:#?}"
        );
    }

    #[test]
    fn an_ancestor_of_a_mark_is_drawn_as_a_block_filled_to_the_marked_share() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Expand);
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Mark);
        let frame = painted(&mut view, 100, 8);

        // One of the root's two claims is marked, and the other has no price, so the share is
        // stated in claims: half, which is the fourth of seven blocks. A bare `[~]` would say
        // "some of this" and stop there; the glyph says how much, which is what a reader
        // deciding whether to open the row actually needs.
        let root = &frame[1];
        assert!(root.contains("[▄]"), "{frame:#?}");
        assert!(!root.contains("[x]"), "{frame:#?}");
    }

    #[test]
    fn the_confirmation_says_what_it_will_delete_and_what_it_will_not() {
        let mut view = view();
        view.ask(Pending {
            targets: vec!["/scan/nx/node_modules".into()],
            bytes: 2 * 1024 * 1024,
            unpriced: 1,
            kept: vec!["/scan/old/target: it holds a git checkout".to_owned()],
            answer: Answer::Delete,
        });
        let frame = painted(&mut view, 100, 20);
        let box_text = frame.join("\n");

        assert!(
            box_text.contains("Delete 1 directory, giving back 2.0 MiB?"),
            "{box_text}"
        );
        assert!(box_text.contains("carry no price"), "{box_text}");
        assert!(box_text.contains("holds a git checkout"), "{box_text}");
        assert!(box_text.contains("cancel"), "{box_text}");
        assert!(box_text.contains("delete"), "{box_text}");
    }

    #[test]
    fn the_help_overlay_is_the_keymap_itself() {
        let mut view = view();
        view.apply(Action::Help);
        let frame = painted(&mut view, 100, 30).join("\n");

        assert!(frame.contains("Everywhere"), "{frame}");
        assert!(frame.contains("quit"), "{frame}");
        assert!(frame.contains("mark this row's whole subtree"), "{frame}");
        // The one key that writes says out loud that it asks first.
        assert!(
            frame.contains("delete what is marked — asks first"),
            "{frame}"
        );
    }

    #[test]
    fn a_narrow_terminal_drops_the_regeneration_column_rather_than_the_path() {
        let mut view = view();
        let frame = painted(&mut view, 48, 8);
        let row = frame.iter().find(|line| line.contains("nx")).unwrap();
        assert!(row.contains("2.0 MiB"), "{row}");
        assert!(!row.contains("install"), "{row}");
    }

    #[test]
    fn a_claim_a_pricing_thread_is_inside_shimmers_where_its_dash_would_be() {
        let mut tree = Tree::new("/scan");
        tree.insert(hit("/scan/one/node_modules", Size::Unmeasured, 0));
        tree.insert(hit("/scan/two/target", Size::Unmeasured, 0));
        let mut view = View::new(tree);
        open_everything(&mut view);
        view.pricing(Path::new("/scan/one/node_modules"));
        let frame = painted(&mut view, 100, 10);

        // Exactly as many rows shimmer as the pool has threads working, which is the honest
        // reading of "which of these dashes is being worked on right now". The other is
        // queued, and a dash that will be measured in four minutes is a different fact about
        // a row from one being measured this instant.
        assert!(row_with(&frame, "node_modules").contains('━'), "{frame:#?}");
        let queued = row_with(&frame, "target");
        assert!(queued.contains('—'), "{frame:#?}");
        assert!(!queued.contains('━'), "{frame:#?}");
    }

    #[test]
    fn an_ancestor_that_is_still_being_priced_draws_its_number_as_a_floor() {
        let mut tree = Tree::new("/scan");
        tree.insert(priced("/scan/nx/a/node_modules", 2 * 1024 * 1024));
        tree.insert(hit("/scan/nx/b/target", Size::Unmeasured, 0));
        let mut view = View::new(tree);
        let frame = painted(&mut view, 100, 10);

        // `2.0 MiB` on its own would be wrong in the one direction a cleaner must not be
        // wrong in. The `>` is true every moment it is up and costs nothing.
        assert!(row_with(&frame, "nx").contains("> 2.0 MiB"), "{frame:#?}");
    }

    #[test]
    fn a_directory_a_removal_left_standing_is_marked_calmly_rather_than_as_an_error() {
        let mut view = view();
        view.refused(&[Refused {
            path: "/scan/old/target".into(),
            reason: Refusal::HoldsCheckout,
        }]);
        open_everything(&mut view);
        let frame = painted(&mut view, 110, 10);

        // The safety model refusing a subtree is the tool working. It says which directory
        // and why, on the row, where the footer's count cannot.
        let kept = row_with(&frame, "target");
        assert!(kept.contains("kept — holds a git checkout"), "{frame:#?}");
        // And it does not take the row the regeneration command would have had, on a row
        // that has one — the newer and stranger fact wins.
        assert!(
            !row_with(&frame, "node_modules").contains("kept"),
            "{frame:#?}"
        );
    }

    #[test]
    fn the_footer_says_where_the_deleter_is_and_not_only_what_it_has_given_back() {
        let mut view = view();
        view.ask(Pending {
            targets: vec![
                "/scan/nx/node_modules".into(),
                "/scan/old/target".into(),
                "/scan/gone".into(),
                "/scan/also-gone".into(),
            ],
            bytes: 2 * 1024 * 1024,
            unpriced: 0,
            kept: Vec::new(),
            answer: Answer::Delete,
        });
        view.apply(Action::Answer);
        view.removed(Path::new("/scan/nx/node_modules"), 1024 * 1024, true);
        view.animate(std::time::Instant::now());
        let frame = painted(&mut view, 100, 8);

        // A running byte total says how much has gone and nothing about how much is left, so
        // a reader cannot tell a third of the way through from nearly finished. The count
        // against the batch's own size can, and it is beside the bytes rather than instead of
        // them: the pair is what somebody who walked away comes back to read.
        assert!(
            frame[7].contains("removing 1 of 4 directories"),
            "{frame:#?}"
        );
        assert!(frame[7].contains("25%"), "{frame:#?}");
        assert!(frame[7].contains("freed 1.0 MiB"), "{frame:#?}");

        // And the one thing that speaks while a removal runs still gets through. `q` is held
        // back until the batch finishes; a reader who pressed it and saw nothing change would
        // have no way to tell a refusal from a terminal that had stopped listening.
        view.apply(Action::Quit);
        let frame = painted(&mut view, 100, 8);
        assert!(frame[7].contains("removing 1 of 4"), "{frame:#?}");
        assert!(frame[7].contains("the removal has to finish"), "{frame:#?}");
    }

    #[test]
    fn the_footer_keeps_the_freed_total_after_the_notice_has_moved_on() {
        let mut view = view();
        view.deleted(
            "removed 2.0 MiB from 1 directory".to_owned(),
            2 * 1024 * 1024,
        );
        view.animate(std::time::Instant::now() + COUNT_UP * 8);
        let frame = painted(&mut view, 100, 8);

        // The number the reader who walked away came back for. It outlives the notice
        // because the notice is about what just happened and this is about the session.
        assert!(frame[7].contains("removed 2.0 MiB"), "{frame:#?}");
        assert!(frame[7].contains("freed 2.0 MiB"), "{frame:#?}");
    }

    #[test]
    fn a_scan_that_could_not_read_everything_says_so_beside_its_own_numbers() {
        let mut view = view();
        let mut terminal = Terminal::new(TestBackend::new(120, 6)).unwrap();
        let errors = vec![crate::walk::WalkError {
            path: Some("/scan/locked".into()),
            message: "Permission denied".to_owned(),
        }];
        terminal
            .draw(|frame| draw(frame, &mut view, &errors))
            .unwrap();
        let header: String = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(header.contains("1 path unread"), "{header}");
        assert!(header.contains("floor"), "{header}");
    }
}
