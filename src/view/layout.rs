use super::*;

pub(super) const MIN_MAIN_WIDTH: u16 = 20;

pub(super) fn centered_rect(width: u16, height: u16, area: Rect) -> Rect {
    let width = width.min(area.width);
    let height = height.min(area.height);
    Rect {
        x: area.x + area.width.saturating_sub(width) / 2,
        y: area.y + area.height.saturating_sub(height) / 2,
        width,
        height,
    }
}

pub(super) fn sidebar_width(cols: u16, configured_width: Option<u16>) -> u16 {
    let width = if let Some(width) = configured_width {
        width
    } else if cols >= 160 {
        72
    } else if cols >= 120 {
        56
    } else {
        cols.saturating_mul(2).saturating_div(5).clamp(20, 42)
    };
    width.min(cols.saturating_sub(MIN_MAIN_WIDTH))
}

pub(super) fn panel_block(title: Line<'static>, highlighted: bool) -> Block<'static> {
    let block = Block::default().borders(Borders::ALL).title(title);
    if highlighted {
        block.border_style(highlight_style())
    } else {
        block
    }
}

pub(super) fn panel_title(key: &'static str, title: &'static str, focused: bool) -> Line<'static> {
    Line::from(Span::styled(
        format!("[{key}] {title}"),
        title_style(focused),
    ))
}
