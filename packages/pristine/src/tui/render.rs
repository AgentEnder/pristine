//! Drawing one frame, and reading back where it put things.
//!
//! Everything here reads the [`View`] and writes cells. The two things it writes *back* are
//! measurements — how many rows the tree pane got, and how far the help page can scroll —
//! and a [`Placed`], which is where the frame put everything a pointer can aim at.
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
//! # The layout is stated once and read twice
//!
//! [`hit`] resolves a cell to a [`Spot`], and it does that against the rectangles [`draw`]
//! actually produced rather than against a second description of them. Being one cell out
//! here is not a cosmetic bug: it is a press that opens, marks or prices a row nobody aimed
//! at. So the tree's columns come out of one [`columns`] call that both lays the table out
//! and answers the hit test, and a row's own cells — the mark box, the indent, the expander —
//! are spelled by the constants [`label`] draws them with.
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
use ratatui::layout::{Alignment, Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span, Text};
use ratatui::widgets::{Block, Borders, Cell, Clear, Paragraph, Row as TableRow, Table, Wrap};

use super::keymap::help;
use super::state::{Answer, Mark, Roll, View, plural};
use super::treemap;
use super::treemap::tiles;
use crate::size::human;
use crate::tree::{NodeId, Order, Sort};
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
/// The blank cells between two columns.
const SPACING: u16 = 2;
/// How many cells the pricing shimmer travels across, in place of the dash.
const SHIMMER: usize = 5;
/// The eight steps a partial mark is filled to, one per eighth.
///
/// Never empty and never full: an empty box is [`Mark::None`] and a full one is [`Mark::All`],
/// so a partial row is always somewhere strictly in between and the glyph says where.
const BLOCKS: [&str; 7] = ["▁", "▂", "▃", "▄", "▅", "▆", "▇"];

/// The cells `[x]` occupies — the mark box, and the whole of what a press can aim at.
const BOX: usize = 3;
/// The box plus the blank that separates it from the indent.
const MARKER: usize = BOX + 1;
/// One level of indent.
const INDENT: usize = 2;

/// Draws the whole frame, and says where it put what a pointer can aim at.
pub fn draw(frame: &mut Frame, view: &mut View, errors: &[WalkError]) -> Placed {
    let [header, body, footer] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
    ])
    .areas(frame.area());

    frame.render_widget(headline(view, errors), header);
    // The map takes its columns off the tree before anything else is laid out, so every
    // rectangle below — the heading, the columns, the rows a press is resolved against — is
    // of the tree's *own* pane rather than of the frame. Nothing is drawn into the map's
    // cells here beyond its caption: the picture arrives afterwards, from outside ratatui,
    // and cells this frame wrote would be cells the image covers.
    let (body, map) = split_off_map(frame, view, body);
    // The heading is a line of the body, so it comes out of the body's height before the
    // view is told how many rows it has — a page size that counted the heading would scroll
    // one row further than the pane can draw. A body with no room for both is all rows: a
    // column name is worth less than the row it would cost.
    let (head, rows) = if body.height > 1 {
        let [head, rows] =
            Layout::vertical([Constraint::Length(1), Constraint::Min(0)]).areas(body);
        (Some(head), rows)
    } else {
        (None, body)
    };
    let columns = columns(rows);
    view.viewport(rows.height as usize);
    if let Some(head) = head {
        heading(frame, head, columns, view.sort());
    }
    frame.render_widget(tree(view, columns, rows.height as usize), rows);
    frame.render_widget(status(view), footer);

    let mut placed = Placed {
        columns: Some(columns),
        heading: head,
        rows,
        map,
        scroll: view.scroll(),
        overlay: None,
        answers: None,
    };

    // Where each overlay was drawn, kept rather than assigned as it is drawn: the drawing
    // order is bottom-up and [`Placed::overlay`] holds the **topmost** one, so the two would
    // disagree the moment a prompt is opened over a question.
    let mut prompt_at = None;
    let mut confirm_at = None;
    let mut help_at = None;

    if view.prompt().is_some() {
        prompt_at = Some(prompting(frame, view, footer));
    }
    if view.pending().is_some() {
        confirm_at = Some(confirming(frame, view));
    }
    if view.help().is_some() {
        help_at = Some(helping(frame, view));
    }

    // The same ranking [`View::overlay`] gives the keyboard, so a click and a keystroke
    // cannot be told that different surfaces are in front.
    placed.overlay = prompt_at.or(help_at).or(confirm_at.map(|(area, _)| area));
    placed.answers = confirm_at.map(|(_, answers)| answers);
    placed
}

/// Takes the map's columns off the right of the body, when there is a map and room for one.
///
/// The caption is drawn here as ordinary terminal text rather than painted into the image:
/// it is the one line of the pane a reader might want to select or copy, and text the
/// terminal drew is text at the terminal's own font size.
fn split_off_map(frame: &mut Frame, view: &View, body: Rect) -> (Rect, Option<Rect>) {
    if !view.maps() || body.height < treemap::MIN_HEIGHT {
        return (body, None);
    }
    let Some(width) = treemap::Pane::width_in(body.width) else {
        return (body, None);
    };
    let [tree, gap, map] = Layout::horizontal([
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(width),
    ])
    .areas(body);
    let _ = gap;
    let [caption, picture] =
        Layout::vertical([Constraint::Length(1), Constraint::Min(1)]).areas(map);
    let said = tiles::focus(view).map_or_else(String::new, |root| tiles::caption(view, root));
    frame.render_widget(
        Paragraph::new(Line::from(Span::styled(
            said,
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )))
        .style(Style::default().bg(Color::Rgb(24, 24, 30))),
        caption,
    );
    (tree, Some(picture))
}

/// The filter prompt, over the footer it borrows.
fn prompting(frame: &mut Frame, view: &View, footer: Rect) -> Rect {
    let Some(prompt) = view.prompt() else {
        return footer;
    };
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
    // The caret is the terminal's own, so a reader's cursor is where they are typing rather
    // than drawn as a block that their terminal's blink rate disagrees with.
    frame.set_cursor_position((
        footer.x + 1 + u16::try_from(prompt.caret()).unwrap_or(u16::MAX),
        footer.y,
    ));
    footer
}

/// The question, and where its two answers went.
fn confirming(frame: &mut Frame, view: &View) -> (Rect, [Rect; 2]) {
    let Some(pending) = view.pending() else {
        return (Rect::default(), [Rect::default(); 2]);
    };
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
    // The answers get a line of the box rather than a line of the paragraph, and that is a
    // correctness change rather than a tidying one: the lines above them **wrap**, so one
    // long refusal would push a button a row down from where a press was told it is.
    let area = centred(
        frame.area(),
        66,
        u16::try_from(lines.len()).unwrap_or(8) + 3,
    );
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" this cannot be undone ")
        .border_style(Style::default().fg(Color::Red));
    let inner = block.inner(area);
    frame.render_widget(Clear, area);
    frame.render_widget(block, area);
    let [said, asked] = Layout::vertical([Constraint::Min(0), Constraint::Length(1)]).areas(inner);
    frame.render_widget(Paragraph::new(lines).wrap(Wrap { trim: true }), said);
    (area, buttons(frame, asked, pending.answer))
}

/// The generated key reference, and how far down it the reader is.
fn helping(frame: &mut Frame, view: &mut View) -> Rect {
    let area = centred(frame.area(), 74, frame.area().height.saturating_sub(4));
    let page = help_page();
    let height = area.height.saturating_sub(2) as usize;
    view.clamp_help(page.lines.len().saturating_sub(height));
    let at = view.help().unwrap_or(0);
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
    area
}

/// A confirmation's two answers, drawn into a line of their own.
///
/// Returns where they went, in [`Answer::ALL`] order, from the same widths they were drawn
/// with — so "which button is this" is decided once and read by both halves.
fn buttons(frame: &mut Frame, area: Rect, chosen: Answer) -> [Rect; 2] {
    /// The blank between the two, which belongs to neither: a press that lands here is a
    /// press on the box, and the box does nothing.
    const GAP: u16 = 3;

    let mut at = area.x;
    Answer::ALL.map(|answer| {
        let label = format!("  {}  ", answer.label());
        let width = u16::try_from(label.chars().count()).unwrap_or(u16::MAX);
        let rect = Rect {
            x: at,
            y: area.y,
            width: width.min(area.right().saturating_sub(at)),
            height: 1,
        };
        at = at.saturating_add(width + GAP);
        frame.render_widget(
            Paragraph::new(Span::styled(label, answered(answer == chosen))),
            rect,
        );
        rect
    })
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

/// Where the tree's columns went, laid out once for the table and for the hit test.
///
/// `Length` constraints derived from these are what the [`Table`] is given, so the widget's
/// own split reproduces this one exactly rather than merely agreeing with it — which is the
/// difference between a heading a click sorts by and a heading a click sorts *near*.
///
/// The rectangles cover the tree's **rows**. The heading is a line above them and takes only
/// the horizontal extent, which is the whole of what a column is: `x` and `width` are the
/// column, and `y` is whichever band asked for it.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Columns {
    /// The mark box, the indent, the expander and the name.
    pub name: Rect,
    /// What the subtree is worth.
    pub size: Rect,
    /// How long ago it was touched.
    pub age: Rect,
    /// What brings a claim back, when the terminal is wide enough to carry the column.
    pub regenerate: Option<Rect>,
}

/// Splits the tree pane into its columns.
fn columns(area: Rect) -> Columns {
    // A path a reader cannot read is worse than a command they have to press `Enter` on the
    // row to see.
    let wide = area.width >= NARROW;
    let mut widths = vec![
        Constraint::Min(20),
        Constraint::Length(SIZE),
        Constraint::Length(AGE),
    ];
    if wide {
        widths.push(Constraint::Length(REGENERATE));
    }
    let split = Layout::horizontal(widths).spacing(SPACING).split(area);
    Columns {
        name: split[0],
        size: split[1],
        age: split[2],
        regenerate: split.get(3).copied(),
    }
}

/// The line of column names above the rows — and the one thing on the screen a click sorts by.
fn heading(frame: &mut Frame, area: Rect, at: Columns, sort: Sort) {
    frame.render_widget(
        Block::default().style(Style::default().bg(Color::Rgb(24, 24, 30))),
        area,
    );
    let named = |order: Order| {
        let mut name = order.column().to_owned();
        if sort.by == order {
            // The footer's own arrow, so the two places that say which way the tree is
            // sorted say it the same way.
            name.push_str(if sort.reverse { " ↑" } else { " ↓" });
        }
        let style = if sort.by == order {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        Span::styled(name, style)
    };
    for (rect, order, align) in [
        (at.name, Order::Path, Alignment::Left),
        (at.size, Order::Size, Alignment::Right),
        (at.age, Order::Age, Alignment::Right),
    ] {
        frame.render_widget(
            Paragraph::new(Line::from(named(order))).alignment(align),
            Rect {
                y: area.y,
                height: 1,
                ..rect
            },
        );
    }
    if let Some(rect) = at.regenerate {
        // Named but not a button: nothing orders a level by the command that rebuilds it, so
        // a press here deliberately does nothing rather than reversing a neighbour the
        // reader did not aim at. See [`heading_at`].
        frame.render_widget(
            Paragraph::new(Span::styled(
                "how to get it back",
                Style::default().fg(Color::Rgb(70, 70, 84)),
            )),
            Rect {
                y: area.y,
                height: 1,
                ..rect
            },
        );
    }
}

/// The tree itself: one table row per *visible* row, from the scroll offset down.
///
/// Bounded by the pane rather than by the tree, which is the difference between drawing a screen
/// and building one. A home directory fully expanded is 32,634 rows; the widget would draw the
/// first `height` of them either way, but every row handed to it is a `Vec` of styled spans
/// allocated first and thrown away second.
fn tree(view: &View, at: Columns, height: usize) -> Table<'static> {
    let wide = at.regenerate.is_some();
    let rows: Vec<TableRow> = view
        .rows()
        .iter()
        .enumerate()
        .skip(view.scroll())
        .take(height)
        .map(|(index, row)| {
            let node = view.tree().node(row.id);
            let selected = view.cursor() == Some(index);
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

    // The widths the split above produced, handed back as fixed lengths: with every
    // constraint a `Length` the table's own `Layout::horizontal` can only reproduce them,
    // which is what makes [`Columns`] the geometry rather than a guess about it.
    let mut widths = vec![
        Constraint::Length(at.name.width),
        Constraint::Length(at.size.width),
        Constraint::Length(at.age.width),
    ];
    if let Some(regenerate) = at.regenerate {
        widths.push(Constraint::Length(regenerate.width));
    }
    Table::new(rows, widths).column_spacing(SPACING)
}

/// A row's marker, indent, expander and name, as one run of spans.
///
/// The cell offsets are [`BOX`], [`MARKER`] and [`INDENT`] rather than literals, because
/// [`zone_at`] resolves a press by them: a mark box drawn one cell wider than the hit test
/// believes is a click that selects where it meant to mark.
fn label(view: &View, id: NodeId, depth: usize) -> Vec<Span<'static>> {
    let node = view.tree().node(id);
    let name = if node.parent.is_none() {
        node.path.display().to_string()
    } else {
        node.name.to_string_lossy().into_owned()
    };
    vec![
        marker(view, id),
        // Spelled with [`INDENT`] rather than a literal pair of spaces, because [`zone_at`]
        // resolves a press by that same constant: an indent drawn a cell wider than the hit
        // test believes is a click that lands on the row beside the one it aimed at.
        Span::raw(" ".repeat(INDENT * depth)),
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

/// How one of a confirmation's two answers is drawn, highlighted or not.
fn answered(highlighted: bool) -> Style {
    if highlighted {
        Style::default()
            .fg(Color::Black)
            .bg(Color::White)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
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

/// Where the last frame put everything a pointer can aim at.
///
/// Read back from the layout [`draw`] produced rather than described a second time. A
/// hard-coded row-to-`y` map would be one line out the first time the header or the heading
/// changed height, and being one line out here presses a row nobody chose — on a screen whose
/// keys delete directories.
///
/// [`Default`] is a frame that was never drawn, on which every press resolves to
/// [`Spot::Nowhere`]. That is the honest answer for the first mouse event of a run and for a
/// test that has not painted: a synthetic geometry would be a second layout, which is the
/// thing this type exists to avoid.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Placed {
    /// The tree's columns, or `None` on a frame that drew no tree.
    pub columns: Option<Columns>,
    /// The heading line, when the pane was tall enough to spend a row on one.
    pub heading: Option<Rect>,
    /// The body rows.
    pub rows: Rect,
    /// Where the treemap's image goes, when the pane is up. `None` is a frame with no map on
    /// it, which is every frame in a terminal that cannot draw one.
    pub map: Option<Rect>,
    /// Which row of the tree the top drawn row held.
    ///
    /// Recorded rather than re-read from the view, so a press maps `y` to a row through the
    /// offset the rows were **drawn** at — a sync between the frame and the press would
    /// otherwise shift every row by the difference.
    pub scroll: usize,
    /// The topmost overlay's box, in the ranking [`super::state::View::overlay`] uses. One
    /// slot, because a press lands on one thing.
    pub overlay: Option<Rect>,
    /// A confirmation's two answers, in [`Answer::ALL`] order, where they were drawn.
    ///
    /// Inside [`overlay`](Self::overlay) and checked first. `None` whenever no question was
    /// on the frame — which is what a press made *before* the box appeared lands on, and is
    /// the whole of what stops such a press from answering one. See
    /// [`super::keymap::finish`].
    pub answers: Option<[Rect; 2]>,
}

/// Which part of a row the pointer is on.
///
/// pristine's rows carry a target pua's do not: the mark box. A box that is drawn and cannot
/// be pressed is a lie the pointer tells about itself, so it is a zone of its own rather than
/// part of the name.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Zone {
    /// The `[x]` box — mark this row's subtree, or unmark it.
    Mark,
    /// The `▸`/`▾` indicator — open the row, or close it.
    ///
    /// A leaf leaves this cell blank, and a blank cell cannot have been aimed at: opening a
    /// row with nothing under it is a no-op in [`super::state::View`] rather than a case the
    /// hit test has to know about.
    Open,
    /// The indent, the name, and everything past it — put the cursor here.
    Name,
}

/// What is under the pointer, on the frame that was drawn.
///
/// The routing chain the keymap states for keys — overlay first, then the tree, then the
/// globals — falls out of resolving this **geometrically** rather than being re-implemented:
/// an overlay covers the screen it is over, so a press inside one cannot also be a press on
/// the tree, and one outside is the dismissal. [`super::keymap::pointer`] turns a spot and a
/// gesture into an action; nothing else has to know the order.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Spot {
    /// A column heading, over the column that orders a level by this key.
    Heading(Order),
    /// A tree row, named by the **directory** on it rather than by its position.
    ///
    /// The identity rule this whole model turns on. Rows re-sort as prices land and *vanish*
    /// as removals complete, so a row index resolved at the press and acted on at the release
    /// names a different directory — and the action on the other end of it deletes one. A
    /// [`NodeId`] is not that index: [`crate::tree::Tree`] only ever pushes a new slot and
    /// never recycles a detached one, so an id names one directory for the life of the run,
    /// and a press on a row that has since been removed resolves to a row that is no longer
    /// on screen and does nothing at all.
    Row {
        /// Which directory.
        id: NodeId,
        /// Which part of its row.
        zone: Zone,
    },
    /// The tree pane, past the end of its rows.
    Tree,
    /// Inside the help overlay.
    Help,
    /// On one of a confirmation's two answers.
    Answer(Answer),
    /// Inside a confirmation, and not on either answer.
    ///
    /// The question, the consequence and the padding around them are things to read rather
    /// than things to press, so landing near an answer is a miss and does nothing. What
    /// guards the irreversible press is the gesture — down and up in the same button — rather
    /// than the absence of a target; see [`super::keymap::finish`].
    Confirm,
    /// Inside the filter prompt.
    Prompt,
    /// Outside the overlay that is up — where a press dismisses it.
    Outside,
    /// Chrome, or a frame with nothing on it: the header line and the footer.
    Nowhere,
}

/// What the pointer is over, resolved against the frame that was drawn.
///
/// Takes the view as well as the frame because two of the answers are about content rather
/// than geometry: which directory a row is, so that a re-sort between the press and the
/// release cannot select a stranger, and how deep it is, so that the expander's cell is the
/// one it was drawn in.
#[must_use]
pub fn hit(view: &View, placed: &Placed, at: Position) -> Spot {
    // The overlay first, and by **omission** rather than by a branch: while one is up nothing
    // below this returns, so there is no arrangement in which a press reaches the tree behind
    // it.
    if let Some(over) = placed.overlay {
        if !over.contains(at) {
            return Spot::Outside;
        }
        // In the ranking the keyboard is routed by, so the surface a press lands on is the
        // one a keystroke would reach.
        if view.prompt().is_some() {
            return Spot::Prompt;
        }
        if view.help().is_some() {
            return Spot::Help;
        }
        return answer_at(placed.answers, at).map_or(Spot::Confirm, Spot::Answer);
    }

    let Some(columns) = placed.columns else {
        return Spot::Nowhere;
    };
    if placed.heading.is_some_and(|head| head.contains(at)) {
        return heading_at(columns, at.x);
    }
    if placed.rows.contains(at) {
        let index = placed.scroll + usize::from(at.y - placed.rows.y);
        return match view.rows().get(index) {
            Some(row) => Spot::Row {
                id: row.id,
                zone: zone_at(columns, row.depth, at.x),
            },
            None => Spot::Tree,
        };
    }
    Spot::Nowhere
}

/// Which of a confirmation's answers the pointer is on, if either.
fn answer_at(answers: Option<[Rect; 2]>, at: Position) -> Option<Answer> {
    Answer::ALL
        .into_iter()
        .zip(answers?)
        .find(|(_, rect)| rect.contains(at))
        .map(|(answer, _)| answer)
}

/// Which ordering the heading holds at cell `x`.
///
/// Each gap belongs to the column before it, which is where that heading's trailing blanks
/// are anyway. The regeneration column is the one part of the line that is *not* a button:
/// nothing orders a level by the command that rebuilds it, so a press there is a miss rather
/// than a reversal of the neighbour the reader did not aim at.
fn heading_at(at: Columns, x: u16) -> Spot {
    if x < at.size.x {
        return Spot::Heading(Order::Path);
    }
    if x < at.age.x {
        return Spot::Heading(Order::Size);
    }
    match at.regenerate {
        Some(regenerate) if x >= regenerate.x => Spot::Nowhere,
        _ => Spot::Heading(Order::Age),
    }
}

/// Which part of a row's name column cell `x` is in.
///
/// Walked through the same offsets [`label`] draws with, so each target is where its glyph
/// is rather than where a second description says it should be.
fn zone_at(at: Columns, depth: usize, x: u16) -> Zone {
    let offset = usize::from(x.saturating_sub(at.name.x));
    if offset < BOX {
        return Zone::Mark;
    }
    if offset == MARKER + INDENT * depth {
        return Zone::Open;
    }
    Zone::Name
}

#[cfg(test)]
mod tests {
    use super::{INDENT, MARKER, Placed, Spot, Zone, draw, hit as press_on};
    use crate::delete::{Refusal, Refused};
    use crate::fixture::{hit, priced};
    use crate::size::Size;
    use crate::tree::{Order, Tree};
    use crate::tui::keymap::{Action, Motion};
    use crate::tui::moving::COUNT_UP;
    use crate::tui::state::{Answer, Pending, View};
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;
    use ratatui::layout::Position;
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
        frame_of(view, width, height).0
    }

    /// The same frame, with the geometry it reported — for the tests about pressing on it.
    fn frame_of(view: &mut View, width: u16, height: u16) -> (Vec<String>, Placed) {
        let mut terminal = Terminal::new(TestBackend::new(width, height)).unwrap();
        let mut placed = Placed::default();
        terminal
            .draw(|frame| placed = draw(frame, view, &[]))
            .unwrap();
        let buffer = terminal.backend().buffer().clone();
        let lines = (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
                    .trim_end()
                    .to_owned()
            })
            .collect();
        (lines, placed)
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
        //
        // Found rather than indexed: the heading is a line of the body, so the root row is no
        // longer whichever line the frame happens to put second.
        let root = frame
            .iter()
            .find(|line| line.starts_with('[') && line.contains("/scan"))
            .unwrap_or_else(|| panic!("{frame:#?}"));
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
        // …and the map's key is on the page by construction rather than by anyone remembering
        // to add it, which is what the table is for.
        assert!(frame.contains("show or hide the map"), "{frame}");
    }

    #[test]
    fn a_narrow_terminal_drops_the_regeneration_column_rather_than_the_path() {
        let mut view = view();
        let frame = painted(&mut view, 48, 8);
        let row = frame.iter().find(|line| line.contains("nx")).unwrap();
        assert!(row.contains("2.0 MiB"), "{row}");
        assert!(!row.contains("install"), "{row}");
    }

    // ---- what a press lands on --------------------------------------------------------

    /// Where the cell holding `glyph` on the row named by `path` actually is.
    ///
    /// The point of the whole test section: the hit test is asked about the cell the frame
    /// *drew* the glyph in, found by looking at the painted buffer, rather than about a cell
    /// a second description of the layout believes it is in.
    fn cell_of(frame: &[String], placed: &Placed, path: &str, glyph: &str) -> Position {
        let y = frame
            .iter()
            .position(|line| line.contains(path))
            .unwrap_or_else(|| panic!("{path} is not on the frame: {frame:#?}"));
        let line = &frame[y];
        let x = line
            .char_indices()
            .position(|(at, _)| line[at..].starts_with(glyph))
            .unwrap_or_else(|| panic!("no {glyph} on {line:?}"));
        let at = Position::new(u16::try_from(x).unwrap(), u16::try_from(y).unwrap());
        assert!(
            placed.rows.contains(at) || placed.heading.is_some_and(|head| head.contains(at)),
            "{glyph} on {path} is at {at:?}, which the frame says is neither a row nor the heading"
        );
        at
    }

    #[test]
    fn a_press_on_a_rows_glyphs_lands_on_the_part_of_it_that_was_drawn_there() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Expand);
        let (frame, placed) = frame_of(&mut view, 100, 10);
        let nx = view.tree().find(std::path::Path::new("/scan/nx")).unwrap();

        // The three targets a row carries, each asked about at the cell its own glyph was
        // painted in. Being one cell out here is a press that marks where it meant to open.
        assert_eq!(
            press_on(&view, &placed, cell_of(&frame, &placed, "nx", "[")),
            Spot::Row {
                id: nx,
                zone: Zone::Mark
            }
        );
        assert_eq!(
            press_on(&view, &placed, cell_of(&frame, &placed, "nx", "▾")),
            Spot::Row {
                id: nx,
                zone: Zone::Open
            }
        );
        assert_eq!(
            press_on(&view, &placed, cell_of(&frame, &placed, "nx", "nx")),
            Spot::Row {
                id: nx,
                zone: Zone::Name
            }
        );
    }

    #[test]
    fn a_leaf_leaves_the_indicators_cell_blank_and_a_press_there_is_a_press_on_the_row() {
        let mut view = view();
        view.apply(Action::Cursor(Motion::Down));
        view.apply(Action::Expand);
        let (frame, placed) = frame_of(&mut view, 100, 10);
        let leaf = view
            .tree()
            .find(std::path::Path::new("/scan/nx/node_modules"))
            .unwrap();

        // The cell the indicator would be in, computed from the row's depth exactly as
        // `zone_at` does. A leaf draws nothing there, so it resolves to `Open` and the view
        // makes it a no-op rather than the hit test having to know about leaves.
        let depth = view
            .rows()
            .iter()
            .find(|row| row.id == leaf)
            .map(|row| row.depth)
            .unwrap();
        let row = u16::try_from(
            frame
                .iter()
                .position(|line| line.contains("node_modules"))
                .unwrap(),
        )
        .unwrap();
        let x = placed.columns.unwrap().name.x + u16::try_from(MARKER + INDENT * depth).unwrap();
        assert_eq!(
            frame[row as usize].chars().nth(usize::from(x)),
            Some(' '),
            "a leaf drew something in the indicator's cell: {:?}",
            frame[row as usize]
        );
        assert_eq!(
            press_on(&view, &placed, Position::new(x, row)),
            Spot::Row {
                id: leaf,
                zone: Zone::Open
            }
        );
    }

    #[test]
    fn a_press_on_a_column_heading_names_the_order_that_column_is_headed_with() {
        let mut view = view();
        let (frame, placed) = frame_of(&mut view, 100, 8);
        let head = placed.heading.unwrap();
        assert_eq!(head.y, 1, "{frame:#?}");

        for (name, order) in [
            ("directory", Order::Path),
            ("size", Order::Size),
            ("age", Order::Age),
        ] {
            let at = cell_of(&frame, &placed, "directory", name);
            assert_eq!(
                press_on(&view, &placed, at),
                Spot::Heading(order),
                "{name} at {at:?}: {:?}",
                frame[1]
            );
        }
        // The one part of the line that is not a button: nothing orders a level by the
        // command that rebuilds it, so a press there does nothing rather than reversing the
        // neighbour the reader did not aim at.
        let regenerate = placed.columns.unwrap().regenerate.unwrap();
        assert_eq!(
            press_on(&view, &placed, Position::new(regenerate.x, head.y)),
            Spot::Nowhere
        );
    }

    #[test]
    fn a_press_past_the_last_row_is_the_pane_and_a_press_on_the_chrome_is_nothing() {
        let mut view = view();
        let (_, placed) = frame_of(&mut view, 100, 12);
        // Three rows in a pane with room for nine.
        assert_eq!(view.rows().len(), 3);
        assert_eq!(
            press_on(&view, &placed, Position::new(4, placed.rows.y + 5)),
            Spot::Tree
        );
        assert_eq!(press_on(&view, &placed, Position::new(4, 0)), Spot::Nowhere);
        assert_eq!(
            press_on(&view, &placed, Position::new(4, 11)),
            Spot::Nowhere
        );
    }

    #[test]
    fn an_overlay_takes_every_press_over_the_screen_it_covers() {
        let mut view = view();
        view.ask(Pending {
            targets: vec!["/scan/nx/node_modules".into()],
            bytes: 2 * 1024 * 1024,
            unpriced: 0,
            kept: Vec::new(),
            answer: Answer::Cancel,
        });
        let (frame, placed) = frame_of(&mut view, 100, 20);
        let [cancel, delete] = placed.answers.unwrap();

        // The buttons, where they were drawn — the rectangles come out of the same widths
        // the spans were rendered with, so a press cannot land near a word instead of on it.
        assert!(frame[delete.y as usize].contains("delete"), "{frame:#?}");
        assert_eq!(
            press_on(&view, &placed, Position::new(cancel.x + 2, cancel.y)),
            Spot::Answer(Answer::Cancel)
        );
        assert_eq!(
            press_on(&view, &placed, Position::new(delete.x + 2, delete.y)),
            Spot::Answer(Answer::Delete)
        );
        // The blank between them belongs to neither, and the box's own text is something to
        // read rather than something to press.
        assert_eq!(
            press_on(&view, &placed, Position::new(cancel.right(), cancel.y)),
            Spot::Confirm
        );
        assert_eq!(
            press_on(&view, &placed, Position::new(delete.x, delete.y - 2)),
            Spot::Confirm
        );
        // …and there is no arrangement in which a press reaches the tree behind it. This is
        // the row `nx` is on, and it is not a row while the question is up.
        assert_eq!(press_on(&view, &placed, Position::new(1, 3)), Spot::Outside);
    }

    // ---- the map pane ------------------------------------------------------------------

    #[test]
    fn the_map_takes_its_columns_off_the_tree_and_only_when_there_is_room_for_both() {
        let mut view = view();
        // A terminal that cannot draw one never reserves the space, whatever the width.
        let (_, plain) = frame_of(&mut view, 140, 20);
        assert_eq!(plain.map, None);
        assert_eq!(plain.rows.width, 140);

        view.allow_maps(true);
        let (frame, placed) = frame_of(&mut view, 140, 20);
        let map = placed.map.unwrap();
        assert!(map.width >= 32, "{map:?}");
        // The tree gave up exactly those columns and no more, and every rectangle a press is
        // resolved against is of the *tree's* pane rather than of the frame.
        assert_eq!(placed.rows.right() + 1 + map.width, 140);
        assert_eq!(placed.columns.unwrap().name.x, placed.rows.x);
        // The caption is terminal text above the picture, one row down from the header.
        assert!(frame[1].contains("/scan"), "{:?}", frame[1]);

        // …and a press in the map is a press on nothing: the picture is not a button in this
        // spike, and a hit test that guessed otherwise would act on a rectangle nobody could
        // aim at yet.
        assert_eq!(
            press_on(&view, &placed, Position::new(map.x + 2, map.y + 2)),
            Spot::Nowhere
        );
    }

    #[test]
    fn a_terminal_with_no_room_for_a_map_is_all_tree() {
        let mut view = view();
        view.allow_maps(true);
        // Too narrow: the map costs the tree the columns it takes, and the tree is the
        // interface.
        assert_eq!(frame_of(&mut view, 99, 30).1.map, None);
        // Too short: rectangles in a pane eight rows tall are a shape, not an answer.
        assert_eq!(frame_of(&mut view, 140, 8).1.map, None);
        // And the reader can always say no.
        view.apply(Action::ToggleMap);
        assert_eq!(frame_of(&mut view, 140, 30).1.map, None);
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
        view.swept();
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
            .draw(|frame| {
                draw(frame, &mut view, &errors);
            })
            .unwrap();
        let header: String = (0..120)
            .map(|x| terminal.backend().buffer()[(x, 0)].symbol())
            .collect();
        assert!(header.contains("1 path unread"), "{header}");
        assert!(header.contains("floor"), "{header}");
    }
}
