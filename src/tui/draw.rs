use ratatui::{
    Frame,
    layout::{Alignment, Constraint, Layout, Rect},
    style::{Color, Modifier, Style, Stylize},
    text::{Line, Span},
    widgets::{Block, BorderType, Clear, LineGauge, Padding, Paragraph, Wrap},
};

use super::{App, Focus, Hit, Kind, images::Images, play_context};
use crate::{api::Item, proto::Cmd};

const GREEN: Color = Color::Rgb(30, 215, 96);
const TILE: Color = Color::Rgb(38, 38, 38);
const TEXT: Color = Color::Rgb(255, 255, 255);
const GRAY: Color = Color::Rgb(179, 179, 179);
const DIM: Color = Color::Rgb(110, 110, 110);
/// Selection in the focused panel, and the remembered selection in the other one.
const SEL_BG: Color = Color::Rgb(78, 78, 78);
const SEL_BG_DIM: Color = Color::Rgb(44, 44, 44);
const BORDER: Color = Color::Rgb(48, 48, 48);
const BORDER_FOCUS: Color = Color::Rgb(150, 150, 150);
const PLACEHOLDER: Color = Color::Rgb(40, 40, 40);

const LIBRARY_W: u16 = 34;
const QUEUE_W: u16 = 36;
const LIBRARY_ROW_H: u16 = 3;
/// Cards stretch to fill the row; cells are about twice as tall as wide, so a W x W/2 area is square.
const CARD_MIN_W: u16 = 20;
const CARD_MAX_IMG_H: u16 = 14;
const TILE_MIN_W: u16 = 24;
const TILE_MAX_COLS: u16 = 4;
const TILE_H: u16 = 3;
const HEADER_H: u16 = 12;
const ROW_H: u16 = 2;

pub fn draw(f: &mut Frame, app: &mut App) {
    app.hits.clear();
    app.images.begin_frame();
    let [top, mid, bar] =
        Layout::vertical([Constraint::Length(3), Constraint::Min(6), Constraint::Length(6)]).areas(f.area());
    let queue_w = if app.show_queue { QUEUE_W } else { 0 };
    let library_w = if app.show_library { LIBRARY_W } else { 0 };
    let [library, main, queue] =
        Layout::horizontal([Constraint::Length(library_w), Constraint::Min(20), Constraint::Length(queue_w)]).areas(mid);
    draw_top(f, app, top);
    if app.show_library {
        draw_library(f, app, library);
    } else {
        // Keeps wheel events over the old library area going to the main page.
        app.lib_area = Rect::default();
    }
    draw_main(f, app, main);
    if app.show_queue {
        draw_queue(f, app, queue);
    } else {
        app.queue_area = Rect::default();
    }
    draw_bar(f, app, bar);
    if app.devices_open {
        draw_devices(f, app, bar);
    }
    if app.help {
        draw_help(f, app);
    }
    app.images.end_frame();
}

fn panel<'a>() -> Block<'a> {
    focus_panel(false)
}

/// Panel border, brighter when the panel has keyboard focus.
fn focus_panel<'a>(focused: bool) -> Block<'a> {
    Block::bordered().border_type(BorderType::Rounded).border_style(Style::new().fg(if focused { BORDER_FOCUS } else { BORDER }))
}

/// Highlights a selected row: background plus a green bar on its left edge.
fn highlight(f: &mut Frame, r: Rect, focused: bool) {
    f.render_widget(Block::new().bg(if focused { SEL_BG } else { SEL_BG_DIM }), r);
    if focused {
        f.render_widget(Paragraph::new("▌\n".repeat(r.height as usize)).fg(GREEN), Rect { width: 1, ..r });
    }
}

fn button(f: &mut Frame, app: &mut App, area: Rect, label: &str, style: Style, hit: Hit) {
    f.render_widget(Paragraph::new(label).style(style).alignment(Alignment::Center), area);
    app.hits.push((area, hit));
}

/// Placeholder tile, overdrawn by the cover once it is downloaded and encoded.
fn cover(f: &mut Frame, images: &mut Images, url: Option<&str>, area: Rect) {
    f.render_widget(Block::new().bg(PLACEHOLDER), area);
    let mid = Rect { y: area.y + area.height / 2, height: 1.min(area.height), ..area };
    f.render_widget(Paragraph::new("♪").fg(DIM).alignment(Alignment::Center), mid);
    images.draw(f, url, area);
}

/// Web player's heart: green when liked, outline otherwise; nothing for what cannot be liked.
fn heart(f: &mut Frame, hits: &mut Vec<(Rect, Hit)>, area: Rect, uri: &str, liked: Option<bool>, outline: bool) {
    let (label, color) = match liked {
        Some(true) => ("♥", GREEN),
        Some(false) if outline => ("♡", GRAY),
        _ => return,
    };
    f.render_widget(Paragraph::new(label).fg(color), area);
    hits.push((area, Hit::Like(uri.to_string())));
}

fn name_style(playing: bool) -> Style {
    Style::new().fg(if playing { GREEN } else { TEXT }).add_modifier(Modifier::BOLD)
}

fn draw_top(f: &mut Frame, app: &mut App, area: Rect) {
    let [back, home, search, status] = Layout::horizontal([
        Constraint::Length(5),
        Constraint::Length(10),
        Constraint::Max(60),
        Constraint::Min(0),
    ])
    .spacing(1)
    .areas(area);
    for (rect, label, hit) in [(back, "‹", Hit::Back), (home, "⌂ Home", Hit::Home)] {
        f.render_widget(panel(), rect);
        button(f, app, panel().inner(rect), label, Style::new().fg(TEXT).bold(), hit);
    }
    let (text, style) = match &app.search {
        Some(term) => (format!("  {term}▏"), Style::new().fg(TEXT)),
        None => ("  What do you want to play?   /".to_string(), Style::new().fg(DIM)),
    };
    f.render_widget(Paragraph::new(text).style(style).block(focus_panel(app.search.is_some())), search);
    app.hits.push((search, Hit::Search));
    let status_text = if app.status.is_empty() { "? keys" } else { app.status.as_str() };
    f.render_widget(Paragraph::new(status_text).fg(if app.status.is_empty() { DIM } else { GRAY }).alignment(Alignment::Right), Rect { y: status.y + 1, height: 1, ..status });
}


/// Connect device picker, above the right end of the player bar like Spotify's.
fn draw_devices(f: &mut Frame, app: &mut App, bar: Rect) {
    let devices = app.devices.clone();
    let n = devices.as_ref().map_or(1, |d| d.len().max(1)) as u16;
    let (w, h) = (44.min(bar.width), n + 4);
    let area = Rect { x: bar.right().saturating_sub(w + 1), y: bar.y.saturating_sub(h), width: w, height: h };
    f.render_widget(Clear, area);
    let block = focus_panel(true).title(" Connect ").title_bottom(Line::from(" Enter play here · Esc close ").right_aligned());
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 1));
    f.render_widget(block, area);
    let Some(devices) = devices.filter(|d| !d.is_empty()) else {
        let text = if app.devices.is_none() { "Loading…" } else { "No devices" };
        f.render_widget(Paragraph::new(text).fg(GRAY), inner);
        return;
    };
    for (i, d) in devices.iter().enumerate() {
        let r = Rect { y: inner.y + i as u16, height: 1, ..inner };
        if i == app.dev_sel {
            highlight(f, r, true);
        }
        let style = if d.active { Style::new().fg(GREEN).bold() } else { Style::new().fg(TEXT) };
        let line = Line::from(vec![
            Span::styled(if d.active { " ♪ " } else { "   " }, style),
            Span::styled(d.name.clone(), style),
            Span::styled(format!("  {}", d.kind), Style::new().fg(DIM)),
        ]);
        f.render_widget(Paragraph::new(line), r);
        app.hits.push((r, Hit::Device(i)));
    }
}

fn draw_help(f: &mut Frame, app: &App) {
    let rows = app.keymap.help();
    let area = f.area().centered(Constraint::Length(72), Constraint::Length(rows.len() as u16 + 4));
    let lines: Vec<Line> = rows
        .iter()
        .map(|(k, d)| Line::from(vec![Span::styled(format!("{k:>22}  "), Style::new().fg(GREEN).bold()), Span::styled(d.as_str(), Style::new().fg(TEXT))]))
        .collect();
    f.render_widget(Clear, area);
    f.render_widget(Paragraph::new(lines).block(focus_panel(true).title(" Keys ").title_bottom(Line::from(" any key to close ").right_aligned()).padding(Padding::vertical(1))), area);
}

fn draw_library(f: &mut Frame, app: &mut App, area: Rect) {
    app.lib_area = area;
    let focused = app.focus == Focus::Library;
    let block = focus_panel(focused).title(Line::from(" Your Library ").bold().fg(TEXT));
    let inner = block.inner(area);
    f.render_widget(block, area);
    let rows = (inner.height / LIBRARY_ROW_H).max(1) as usize;
    app.lib_top = app.lib_top.min(app.lib_sel);
    if app.lib_sel >= app.lib_top + rows {
        app.lib_top = app.lib_sel + 1 - rows;
    }
    for (k, i) in (app.lib_top..app.library.len()).take(rows).enumerate() {
        let r = Rect { y: inner.y + k as u16 * LIBRARY_ROW_H, height: LIBRARY_ROW_H, ..inner };
        if i == app.lib_sel {
            highlight(f, r, focused);
        }
        let r = Rect { x: r.x + 1, width: r.width.saturating_sub(1), ..r };
        row(f, &mut app.images, &app.library[i], r, 6, false);
        app.hits.push((r, Hit::Library(i)));
    }
}

/// Thumbnail + name + subtitle row, used by the library and track lists.
fn row(f: &mut Frame, images: &mut Images, item: &Item, r: Rect, thumb_w: u16, playing: bool) {
    let [thumb, _, text] =
        Layout::horizontal([Constraint::Length(thumb_w), Constraint::Length(1), Constraint::Min(1)]).areas(r);
    cover(f, images, item.image.as_deref(), thumb);
    let lines = vec![
        Line::from(Span::styled(item.name.as_str(), name_style(playing))),
        Line::from(Span::styled(item.sub.as_str(), Style::new().fg(GRAY))),
    ];
    let pad = r.height.saturating_sub(2) / 2;
    f.render_widget(Paragraph::new(lines), Rect { y: text.y + pad, height: text.height - pad, ..text });
}

#[derive(Clone, Copy, PartialEq)]
enum Blk {
    Header,
    Title(usize),
    Shelf(usize),
    /// One row of tiles of a grid section: (section, row).
    Tiles(usize, usize),
    Row(usize, usize),
    Loading,
}

/// Shelf geometry for a given width: (cards per row, card width, cover height).
fn card_geometry(width: u16) -> (usize, u16, u16) {
    let n = (width / CARD_MIN_W).max(1);
    let card_w = width / n;
    (n as usize, card_w, ((card_w.saturating_sub(2)) / 2).min(CARD_MAX_IMG_H))
}

/// Lyrics in the main area, the current line green and kept a third from the top.
fn draw_lyrics(f: &mut Frame, app: &mut App, area: Rect) {
    let block = focus_panel(app.focus == Focus::Main).title(Line::from(format!(" Lyrics · {} ", app.np.name)).bold().fg(TEXT));
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(2, 1));
    f.render_widget(block, area);
    let lines = match &app.lyrics {
        _ if app.np.uri.is_empty() => return f.render_widget(Paragraph::new("Nothing playing").fg(GRAY), inner),
        Some((uri, lines)) if *uri == app.np.uri => lines,
        _ => return f.render_widget(Paragraph::new("Loading…").fg(GRAY), inner),
    };
    if lines.is_empty() {
        return f.render_widget(Paragraph::new("No lyrics for this track").fg(GRAY), inner);
    }
    let synced = lines.iter().any(|l| l.start_ms > 0);
    let pos = app.position();
    let current = if synced { lines.iter().rposition(|l| l.start_ms <= pos) } else { None };
    let follow = current.unwrap_or(0).saturating_sub(inner.height as usize / 3);
    let top = match app.lyrics_scroll {
        // Unsynced lyrics have nothing to follow: a manual position stays.
        Some((top, at)) if current.is_none() || at.elapsed() < super::LYRICS_FOLLOW => top,
        _ => follow,
    };
    app.lyrics_top = top;
    let text: Vec<Line> = lines
        .iter()
        .enumerate()
        .skip(top)
        .take(inner.height as usize)
        .map(|(i, l)| {
            let style = match current {
                Some(c) if i == c => Style::new().fg(GREEN).bold(),
                Some(c) if i < c => Style::new().fg(DIM),
                _ => Style::new().fg(TEXT),
            };
            // Blank lines in lyrics mark instrumental breaks.
            Line::from(if l.words.is_empty() { "♪".to_string() } else { l.words.clone() }).style(style)
        })
        .collect();
    f.render_widget(Paragraph::new(text).alignment(Alignment::Center), inner);
}

fn draw_main(f: &mut Frame, app: &mut App, area: Rect) {
    if app.show_lyrics {
        return draw_lyrics(f, app, area);
    }
    let title = app.page().map(|p| format!(" {} ", p.title)).unwrap_or_default();
    let focused = app.focus == Focus::Main;
    let block = focus_panel(focused).title(Line::from(title).bold().fg(TEXT));
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    let playing = app.np.uri.clone();
    let Some(page) = app.stack.last_mut() else {
        f.render_widget(Paragraph::new("Loading…").fg(GRAY), inner);
        return;
    };

    let (per_shelf, card_w, img_h) = card_geometry(inner.width);
    let tile_cols = (inner.width / TILE_MIN_W).clamp(1, TILE_MAX_COLS);
    let tile_w = inner.width / tile_cols;
    let height = |b: Blk| match b {
        Blk::Header => HEADER_H + 1,
        Blk::Title(_) | Blk::Loading => 2,
        Blk::Shelf(_) => img_h + 4,
        Blk::Tiles(..) => TILE_H + 1,
        Blk::Row(..) => ROW_H,
    };

    let mut blocks = vec![];
    if page.header.is_some() {
        blocks.push(Blk::Header);
    }
    for (s, sec) in page.secs.iter_mut().enumerate() {
        if !sec.title.is_empty() {
            blocks.push(Blk::Title(s));
        }
        match sec.kind {
            Kind::Shelf => blocks.push(Blk::Shelf(s)),
            Kind::List => blocks.extend((0..sec.items.len()).map(|i| Blk::Row(s, i))),
            Kind::Grid => {
                sec.cols = tile_cols as usize;
                blocks.extend((0..sec.items.len().div_ceil(sec.cols)).map(|r| Blk::Tiles(s, r)));
            }
        }
    }
    if page.loading {
        blocks.push(Blk::Loading);
    }
    if blocks.is_empty() {
        f.render_widget(Paragraph::new("Nothing here.").fg(GRAY), inner);
        return;
    }

    // Scroll so the selected block is fully visible, keeping its section title when possible.
    let (s, i) = page.sel;
    let cols = page.secs.get(s).map_or(1, |sec| sec.cols.max(1));
    let sel = blocks
        .iter()
        .position(|b| *b == Blk::Shelf(s) || *b == Blk::Row(s, i) || *b == Blk::Tiles(s, i / cols))
        .unwrap_or(0);
    if sel < page.top {
        page.top = sel;
    }
    if page.top > 0 && matches!(blocks[page.top - 1], Blk::Title(_) | Blk::Header) && page.top == sel {
        page.top -= 1;
    }
    while page.top < sel && blocks[page.top..=sel].iter().map(|b| height(*b)).sum::<u16>() > inner.height {
        page.top += 1;
    }
    // Snap back to the very top only when the selection is still fully visible from there.
    if blocks[..page.top].iter().all(|b| matches!(b, Blk::Header | Blk::Title(_)))
        && blocks[..=sel].iter().map(|b| height(*b)).sum::<u16>() <= inner.height
    {
        page.top = 0;
    }

    let mut y = inner.y;
    let bottom = inner.y + inner.height;
    let mut hits = vec![];
    for (k, blk) in blocks.iter().enumerate().skip(page.top) {
        let h = height(*blk);
        // A section title is only drawn together with the block that follows it.
        let next = blocks.get(k + 1).filter(|_| matches!(blk, Blk::Title(_))).map_or(0, |b| height(*b));
        if y + h + next > bottom {
            break;
        }
        let r = Rect { y, height: h, ..inner };
        match *blk {
            Blk::Header => {
                let header = page.header.as_ref().unwrap();
                let [img, _, text] = Layout::horizontal([
                    Constraint::Length(HEADER_H * 2),
                    Constraint::Length(2),
                    Constraint::Min(10),
                ])
                .areas(Rect { height: HEADER_H, ..r });
                cover(f, &mut app.images, header.image.as_deref(), img);
                let lines = vec![
                    Line::from(header.kind().to_uppercase()).fg(GRAY),
                    Line::from(""),
                    Line::from(header.name.as_str()).fg(TEXT).bold(),
                    Line::from(header.sub.as_str()).fg(GRAY),
                ];
                let [info, _, play] =
                    Layout::vertical([Constraint::Min(4), Constraint::Length(1), Constraint::Length(1)]).areas(text);
                f.render_widget(Paragraph::new(lines), Rect { y: info.y + info.height.saturating_sub(4), ..info });
                let hint = Rect { x: play.x + 11, width: 1.min(play.width.saturating_sub(11)), ..play };
                let like = Rect { x: play.x + 14, width: 1.min(play.width.saturating_sub(14)), ..play };
                let play = Rect { width: 10.min(play.width), ..play };
                f.render_widget(Paragraph::new(" ▶  Play ").style(Style::new().fg(Color::Black).bg(GREEN).bold()), play);
                f.render_widget(Paragraph::new("P").fg(DIM), hint);
                hits.push((play, Hit::Cmd(play_context(header.uri.clone()))));
                heart(f, &mut hits, like, &header.uri, super::liked(&app.saved, &app.library, &header.uri), true);
            }
            Blk::Loading => {
                f.render_widget(Paragraph::new("Loading…").fg(GRAY), Rect { y: y + 1, height: 1, ..r });
            }
            Blk::Title(s) => {
                let title = Line::from(page.secs[s].title.as_str()).fg(TEXT).bold();
                f.render_widget(Paragraph::new(title), Rect { y: y + 1, height: 1, ..r });
            }
            Blk::Tiles(s, row_idx) => {
                let sec = &page.secs[s];
                for (k, idx) in (row_idx * sec.cols..sec.items.len()).take(sec.cols).enumerate() {
                    let item = &sec.items[idx];
                    let tile = Rect { x: r.x + k as u16 * tile_w, y, width: tile_w.saturating_sub(1), height: TILE_H };
                    let selected = page.sel == (s, idx);
                    f.render_widget(Block::new().bg(if selected { if focused { SEL_BG } else { SEL_BG_DIM } } else { TILE }), tile);
                    let [thumb, _, text] =
                        Layout::horizontal([Constraint::Length(TILE_H * 2), Constraint::Length(1), Constraint::Min(1)]).areas(tile);
                    cover(f, &mut app.images, item.image.as_deref(), thumb);
                    let name = Span::styled(item.name.as_str(), name_style(item.uri == playing));
                    f.render_widget(Paragraph::new(name), Rect { y: y + TILE_H / 2, height: 1, ..text });
                    if selected && focused {
                        f.render_widget(Paragraph::new("▌").fg(GREEN), Rect { x: tile.right().saturating_sub(1), y: y + TILE_H / 2, width: 1, height: 1 });
                    }
                    hits.push((tile, Hit::Main(s, idx)));
                }
            }
            Blk::Shelf(s) => {
                let visible = per_shelf;
                let sec = &mut page.secs[s];
                if page.sel.0 == s {
                    sec.off = sec.off.min(page.sel.1);
                    if page.sel.1 >= sec.off + visible {
                        sec.off = page.sel.1 + 1 - visible;
                    }
                }
                for (k, idx) in (sec.off..sec.items.len()).take(visible).enumerate() {
                    let item = &sec.items[idx];
                    let card = Rect { x: r.x + k as u16 * card_w, y, width: card_w - 2, height: img_h + 3 };
                    if page.sel == (s, idx) {
                        f.render_widget(Block::new().bg(if focused { SEL_BG } else { SEL_BG_DIM }), card);
                        if focused {
                            // The spacing row under the card carries a green underline.
                            let under = Rect { y: card.y + card.height, height: 1, ..card };
                            f.render_widget(Paragraph::new("━".repeat(under.width as usize)).fg(GREEN), under);
                        }
                    }
                    cover(f, &mut app.images, item.image.as_deref(), Rect { height: img_h, ..card });
                    let name = Span::styled(item.name.as_str(), name_style(item.uri == playing));
                    f.render_widget(Paragraph::new(name), Rect { y: y + img_h, height: 1, ..card });
                    // Like Spotify: the playlist description when there is one, else kind and owner/artists.
                    let sub = if item.desc.is_empty() { item.sub.as_str() } else { item.desc.as_str() };
                    f.render_widget(
                        Paragraph::new(sub).fg(GRAY).wrap(Wrap { trim: true }),
                        Rect { y: y + img_h + 1, height: 2, ..card },
                    );
                    hits.push((card, Hit::Main(s, idx)));
                }
            }
            Blk::Row(s, idx) => {
                let item = &page.secs[s].items[idx];
                let is_playing = item.uri == playing;
                let [num, rest] = Layout::horizontal([Constraint::Length(4), Constraint::Min(1)]).areas(r);
                if page.sel == (s, idx) {
                    highlight(f, r, focused);
                }
                let label = if is_playing { "♫".to_string() } else { (idx + 1).to_string() };
                let style = Style::new().fg(if is_playing { GREEN } else { GRAY });
                f.render_widget(Paragraph::new(label).style(style).alignment(Alignment::Right), Rect { y, height: 1, ..num });
                let [text, like] = Layout::horizontal([Constraint::Min(1), Constraint::Length(2)]).areas(rest);
                row(f, &mut app.images, item, Rect { x: text.x + 1, width: text.width.saturating_sub(1), ..text }, 4, is_playing);
                hits.push((r, Hit::Main(s, idx)));
                let liked = super::liked(&app.saved, &app.library, &item.uri);
                heart(f, &mut hits, Rect { y: y + ROW_H / 2, height: 1, width: 1, ..like }, &item.uri, liked, false);
            }
        }
        y += h;
    }
    app.hits.extend(hits);
}

fn draw_queue(f: &mut Frame, app: &mut App, area: Rect) {
    app.queue_area = area;
    let focused = app.focus == Focus::Queue;
    let block = focus_panel(focused).title(Line::from(" Now playing ").bold().fg(TEXT));
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    let np = &app.np;
    let img_h = inner.width / 2;
    let [img, _, info, _, title, list] = Layout::vertical([
        Constraint::Length(img_h),
        Constraint::Length(1),
        Constraint::Length(2),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(0),
    ])
    .areas(inner);
    if np.uri.is_empty() {
        f.render_widget(Paragraph::new("Nothing playing").fg(GRAY), info);
        return;
    }
    cover(f, &mut app.images, Some(np.cover.as_str()), img);
    let lines = vec![Line::from(np.name.as_str()).fg(TEXT).bold(), Line::from(np.artist_line()).fg(GRAY)];
    f.render_widget(Paragraph::new(lines), info);
    f.render_widget(Paragraph::new("Next in queue").fg(TEXT).bold(), title);
    let rows = (list.height / 2).max(1) as usize;
    app.queue_top = app.queue_top.min(app.queue_sel);
    if app.queue_sel >= app.queue_top + rows {
        app.queue_top = app.queue_sel + 1 - rows;
    }
    for (k, i) in (app.queue_top..app.queue.len()).take(rows).enumerate() {
        let r = Rect { y: list.y + k as u16 * 2, height: 2, ..list };
        if i == app.queue_sel && focused {
            highlight(f, r, true);
        }
        let item = &app.queue[i];
        let lines = vec![Line::from(item.name.as_str()).fg(TEXT), Line::from(item.sub.as_str()).fg(DIM)];
        f.render_widget(Paragraph::new(lines), Rect { x: r.x + 1, width: r.width.saturating_sub(1), ..r });
        app.hits.push((r, Hit::Queue(i)));
    }
}

fn draw_bar(f: &mut Frame, app: &mut App, area: Rect) {
    let block = panel();
    let inner = block.inner(area).inner(ratatui::layout::Margin::new(1, 0));
    f.render_widget(block, area);
    let [left, center, right] = Layout::horizontal([
        Constraint::Percentage(30),
        Constraint::Percentage(40),
        Constraint::Percentage(30),
    ])
    .areas(inner);

    // Left: current track.
    let np = app.np.clone();
    if !np.uri.is_empty() {
        let [img, _, text] =
            Layout::horizontal([Constraint::Length(8), Constraint::Length(1), Constraint::Min(1)]).areas(left);
        cover(f, &mut app.images, Some(np.cover.as_str()), img);
        let name = Line::from(np.name.as_str()).fg(TEXT).bold();
        let like_x = text.x + (name.width() as u16 + 1).min(text.width.saturating_sub(1));
        let lines = vec![name, Line::from(np.artist_line()).fg(GRAY)];
        f.render_widget(Paragraph::new(lines), Rect { y: text.y + 1, height: 2, width: text.width.saturating_sub(2), ..text });
        let liked = app.liked(&np.uri);
        heart(f, &mut app.hits, Rect { x: like_x, y: text.y + 1, width: 1, height: 1 }, &np.uri, liked, true);
    }

    // Center: controls and progress.
    let active = |on: bool| Style::new().fg(if on { GREEN } else { GRAY });
    let buttons = [
        ("⇄", active(app.shuffle), Cmd::Shuffle(!app.shuffle)),
        ("⏮", Style::new().fg(TEXT), Cmd::Prev),
        (if np.playing { "⏸" } else { "▶" }, Style::new().fg(Color::Black).bg(TEXT).bold(), Cmd::PlayPause),
        ("⏭", Style::new().fg(TEXT), Cmd::Next),
        (if app.repeat == crate::proto::Repeat::Track { "↻¹" } else { "↻" }, active(app.repeat != crate::proto::Repeat::Off), Cmd::Repeat(app.repeat.cycle())),
    ];
    let total = buttons.len() as u16 * 5 - 2;
    let mut x = center.x + center.width.saturating_sub(total) / 2;
    for (label, style, cmd) in buttons {
        button(f, app, Rect { x, y: center.y, width: 3, height: 1 }, label, style, Hit::Cmd(cmd));
        x += 5;
    }
    let pos = app.position();
    let [elapsed, gauge, duration] =
        Layout::horizontal([Constraint::Length(6), Constraint::Min(5), Constraint::Length(6)])
            .areas(Rect { y: center.y + 2, height: 1, ..center });
    f.render_widget(Paragraph::new(mmss(pos)).fg(GRAY).alignment(Alignment::Right), elapsed);
    f.render_widget(Paragraph::new(mmss(np.duration_ms)).fg(GRAY), duration);
    let gauge = Rect { x: gauge.x + 1, width: gauge.width.saturating_sub(2), ..gauge };
    let ratio = if np.duration_ms > 0 { pos as f64 / np.duration_ms as f64 } else { 0.0 };
    f.render_widget(progress(ratio), gauge);
    app.hits.push((gauge, Hit::Seek));

    // Right: device picker, queue toggle and volume.
    let [_, lyrics, devices, queue, _, icon, volume] = Layout::horizontal([
        Constraint::Min(0),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(3),
        Constraint::Length(1),
        Constraint::Length(3),
        Constraint::Length(16),
    ])
    .areas(Rect { y: right.y + 1, height: 1, ..right });
    button(f, app, lyrics, "♫", active(app.show_lyrics), Hit::Lyrics);
    button(f, app, devices, "⊡", active(app.devices_open), Hit::Devices);
    button(f, app, queue, "≡", active(app.show_queue), Hit::ToggleQueue);
    let vol = np.volume as f64 / u16::MAX as f64;
    let speaker = match vol {
        _ if np.muted => "🔇",
        v if v <= 0.0 => "🔇",
        v if v < 0.34 => "🔈",
        v if v < 0.67 => "🔉",
        _ => "🔊",
    };
    f.render_widget(Paragraph::new(speaker).fg(GRAY), icon);
    app.hits.push((icon, Hit::Mute));
    let bar = progress(vol);
    f.render_widget(if np.muted { bar.filled_style(Style::new().fg(GRAY)) } else { bar }, volume);
    app.hits.push((volume, Hit::Volume));
}

fn progress<'a>(ratio: f64) -> LineGauge<'a> {
    LineGauge::default()
        .ratio(ratio.clamp(0.0, 1.0))
        .label("")
        .filled_symbol("━")
        .unfilled_symbol("━")
        .filled_style(Style::new().fg(TEXT))
        .unfilled_style(Style::new().fg(DIM))
}

fn mmss(ms: u32) -> String {
    format!("{}:{:02}", ms / 60_000, ms / 1000 % 60)
}
