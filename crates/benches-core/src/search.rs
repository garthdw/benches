use anyhow::Result;
use libghostty_vt::Terminal;
use libghostty_vt::point::Space;
use libghostty_vt::search::{MatchBuffer, Search};
use libghostty_vt::selection::Selection;

use crate::grid::{Found, Highlight};

/// A find bar's search over the terminal's scrollback; idle until a query is set.
#[derive(Default)]
pub struct Searcher {
    search: Option<Search<'static, 'static>>,
    buffer: MatchBuffer,
}

/// A match's first and last cell in viewport coordinates.
type Span = ((u16, u32), (u16, u32));

/// `None` while the match is scrolled out of the viewport.
fn span(terminal: &Terminal<'_, '_>, selection: &Selection<'_>) -> Result<Option<Span>> {
    let cell = |grid_ref| -> Result<Option<(u16, u32)>> {
        Ok(terminal
            .point_from_grid_ref(&grid_ref, Space::Viewport)?
            .map(|point| (point.x, point.y)))
    };
    Ok(cell(selection.start())?.zip(cell(selection.end())?))
}

/// One highlight per row a match covers, as a match can wrap.
fn rows(
    ((first_column, first_row), (last_column, last_row)): Span,
    cols: u16,
    current: bool,
) -> impl Iterator<Item = Highlight> {
    (first_row..=last_row).filter_map(move |row| {
        Some(Highlight {
            row: u16::try_from(row).ok()?,
            first: if row == first_row { first_column } else { 0 },
            last: if row == last_row {
                last_column
            } else {
                cols.saturating_sub(1)
            },
            current,
        })
    })
}

impl Searcher {
    /// An empty query ends the search. A new one selects the first match, scrolling to it.
    pub fn set(&mut self, terminal: &mut Terminal<'static, '_>, query: &str) -> Result<()> {
        if query.is_empty() {
            self.search = None;
            return Ok(());
        }
        let search = match &mut self.search {
            Some(search) => search,
            None => self.search.insert(Search::new(terminal)?),
        };
        search.set_needle(terminal, query.as_bytes())?;
        search.run(terminal)?;
        search.select_next(terminal)?;
        Ok(())
    }

    /// Wraps around, and scrolls to the match.
    pub(crate) fn step(&mut self, terminal: &mut Terminal<'static, '_>, older: bool) -> Result<()> {
        if let Some(search) = &mut self.search {
            if older {
                search.select_next(terminal)?;
            } else {
                search.select_prev(terminal)?;
            }
        }
        Ok(())
    }

    /// The matches in view and the count, after catching up with output since the last call.
    pub fn highlights(
        &mut self,
        terminal: &mut Terminal<'static, '_>,
        cols: u16,
    ) -> Result<(Vec<Highlight>, Option<Found>)> {
        let Some(search) = &mut self.search else {
            return Ok((Vec::new(), None));
        };
        search.run(terminal)?;
        let found = Found {
            total: search.total_matches()?,
            selected: search.selected_index()?,
        };
        let snapshot = search.snapshot(terminal)?;
        let terminal = snapshot.terminal();
        let current = match snapshot.selected_match()? {
            Some(selection) => span(terminal, &selection)?,
            None => None,
        };
        let mut highlights = Vec::new();
        for selection in snapshot.viewport_matches(&mut self.buffer)? {
            if let Some(span) = span(terminal, &selection)? {
                highlights.extend(rows(span, cols, Some(span) == current));
            }
        }
        Ok((highlights, Some(found)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::grid::{GridBuilder, Presentation, new_terminal};
    use crate::test_support::scrolled_terminal;
    use libghostty_vt::terminal::ScrollViewport;

    /// The text of the current match and of the other matches in view, and the viewport's rows.
    fn view(
        searcher: &mut Searcher,
        terminal: &mut Terminal<'static, '_>,
    ) -> (String, Vec<String>, Vec<String>, Option<Found>) {
        let (highlights, found) = searcher.highlights(terminal, 20).unwrap();
        let mut builder = GridBuilder::new().unwrap();
        builder.set_search(highlights, found);
        let grid = builder
            .build(terminal, 1, Presentation::default())
            .unwrap()
            .unwrap();
        let mut current = String::new();
        let mut others = Vec::new();
        for (row, line) in grid.cells.chunks(20).enumerate() {
            let text = |current: bool| {
                line.iter()
                    .filter(|cell| cell.bg == Some(crate::grid::match_colors(current).0))
                    .map(|cell| cell.character)
                    .collect::<String>()
            };
            current += &text(true);
            let other = text(false);
            if !other.is_empty() {
                others.push(format!("{row}:{other}"));
            }
        }
        (current, others, grid.lines(), found)
    }

    #[test]
    fn a_query_counts_every_match_in_scrollback() {
        let mut terminal = scrolled_terminal();
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "line1").unwrap();
        let (_, _, _, found) = view(&mut searcher, &mut terminal);
        // line1 and line10 to line19
        assert_eq!(found.map(|found| found.total), Some(11));
    }

    #[test]
    fn a_query_scrolls_to_a_match_and_highlights_it() {
        let mut terminal = scrolled_terminal();
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "line1").unwrap();
        let (current, _, lines, _) = view(&mut searcher, &mut terminal);
        assert_eq!(current, "line1");
        assert!(lines.iter().any(|line| line.starts_with(&current)));
    }

    #[test]
    fn matching_ignores_ascii_case() {
        let mut terminal = new_terminal(20, 3).unwrap();
        terminal.vt_write(b"Hello\r\nhELLO\r\nworld");
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "hello").unwrap();
        let (_, _, _, found) = view(&mut searcher, &mut terminal);
        assert_eq!(found.map(|found| found.total), Some(2));
    }

    #[test]
    fn stepping_visits_every_match_then_wraps() {
        let mut terminal = scrolled_terminal();
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "line1").unwrap();
        let mut seen = Vec::new();
        for _ in 0..11 {
            let (current, _, lines, found) = view(&mut searcher, &mut terminal);
            let line = lines
                .iter()
                .find(|line| line.starts_with(&current) && !current.is_empty())
                .unwrap_or_else(|| panic!("current match not in view: {lines:?}"));
            seen.push((
                found.and_then(|found| found.selected),
                line.trim_end().to_owned(),
            ));
            searcher.step(&mut terminal, true).unwrap();
        }
        let mut lines: Vec<_> = seen.iter().map(|(_, line)| line.clone()).collect();
        lines.sort();
        lines.dedup();
        assert_eq!(lines.len(), 11, "{seen:?}");
        let selected: Vec<_> = seen.iter().map(|(index, _)| *index).collect();
        let (_, _, _, found) = view(&mut searcher, &mut terminal);
        assert_eq!(found.and_then(|found| found.selected), selected[0]);
    }

    #[test]
    fn stepping_back_undoes_stepping_forward() {
        let mut terminal = scrolled_terminal();
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "line1").unwrap();
        let (first, ..) = view(&mut searcher, &mut terminal);
        let (_, _, _, before) = view(&mut searcher, &mut terminal);
        searcher.step(&mut terminal, true).unwrap();
        let (_, _, _, after) = view(&mut searcher, &mut terminal);
        assert_ne!(before, after);
        searcher.step(&mut terminal, false).unwrap();
        let (again, _, _, restored) = view(&mut searcher, &mut terminal);
        assert_eq!((first, before), (again, restored));
    }

    #[test]
    fn other_matches_in_view_are_highlighted_too() {
        let mut terminal = new_terminal(20, 5).unwrap();
        terminal.vt_write(b"ab ab\r\nxx\r\nab");
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "ab").unwrap();
        let (current, others, ..) = view(&mut searcher, &mut terminal);
        assert_eq!(current, "ab");
        assert_eq!(others, ["0:abab"]);
    }

    #[test]
    fn a_match_wrapping_over_two_rows_highlights_both_halves() {
        let mut terminal = new_terminal(20, 3).unwrap();
        terminal.vt_write(b"xxxxxxxxxxxxxxxxxxfoobar");
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "foobar").unwrap();
        let (current, ..) = view(&mut searcher, &mut terminal);
        assert_eq!(current, "foobar");
    }

    #[test]
    fn new_output_that_matches_is_counted() {
        let mut terminal = new_terminal(20, 3).unwrap();
        terminal.vt_write(b"needle\r\n");
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "needle").unwrap();
        terminal.vt_write(b"another needle\r\n");
        let (_, _, _, found) = view(&mut searcher, &mut terminal);
        assert_eq!(found.map(|found| found.total), Some(2));
    }

    #[test]
    fn an_empty_query_ends_the_search_and_clears_the_highlights() {
        let mut terminal = scrolled_terminal();
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "line1").unwrap();
        searcher.set(&mut terminal, "").unwrap();
        let (current, others, _, found) = view(&mut searcher, &mut terminal);
        assert_eq!((current, others, found), (String::new(), Vec::new(), None));
    }

    #[test]
    fn a_query_without_matches_reports_none() {
        let mut terminal = scrolled_terminal();
        terminal.scroll_viewport(ScrollViewport::Top);
        let mut searcher = Searcher::default();
        searcher.set(&mut terminal, "absent").unwrap();
        let (current, _, _, found) = view(&mut searcher, &mut terminal);
        assert_eq!(current, "");
        assert_eq!(
            found,
            Some(Found {
                total: 0,
                selected: None
            })
        );
    }
}
