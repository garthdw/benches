use std::time::Duration;

use rune::tok;
use rune::{El, Key, KeyEvent, TextEdit, Tx, row, text, text_input};

use crate::app::State;
use crate::session::Session;

const FIND_WIDTH: f32 = 180.0;

/// The find bar over the focused terminal. The actor searches; this only holds the query.
#[derive(Default)]
pub(crate) struct Find {
    pub(crate) edit: TextEdit,
    pub(crate) open: bool,
    /// The session searching, which stops when keys move to another.
    pub(crate) session: u64,
    /// The query the session has, so a poll sends only changes.
    sent: String,
}

impl State {
    fn find_session(&self) -> Option<&Session> {
        self.workspace
            .sessions()
            .find(|session| session.id() == self.find.session)
    }
}

pub(crate) fn open_find(state: &mut State, tx: &mut Tx) {
    let Some(session) = state.session().map(Session::id) else {
        return;
    };
    state.find.session = session;
    state.find.open = true;
    tx.focus("find-input", false);
}

fn close_find(state: &mut State) {
    state.find.open = false;
    state.find.sent.clear();
    if let Some(session) = state.find_session() {
        session.search(String::new());
    }
}

fn send_query(state: &mut State) -> bool {
    if state.find.sent == state.find.edit.text {
        return false;
    }
    state.find.sent.clone_from(&state.find.edit.text);
    if let Some(session) = state.find_session() {
        session.search(state.find.sent.clone());
    }
    true
}

/// Sends typing to the session, and closes the bar once keys are elsewhere.
pub(crate) fn poll_find(state: &mut State, active: Option<u64>) -> bool {
    if !state.find.open {
        return false;
    }
    if active != Some(state.find.session) {
        close_find(state);
        return true;
    }
    send_query(state)
}

fn find_key(state: &mut State, event: &KeyEvent, tx: &mut Tx) -> bool {
    match event.key {
        Key::Escape => close_find(state),
        Key::Enter => {
            send_query(state);
            if let Some(session) = state.find_session() {
                session.search_step(!event.mods.shift);
            }
        }
        _ => {
            // A poll after the edit lands sends the query.
            tx.wake_after(Duration::ZERO);
            return false;
        }
    }
    true
}

pub(crate) fn view(state: &State, theme: &tok::Theme) -> Option<El> {
    let session = state.session().filter(|_| state.find.open)?;
    let count = match session.grid().found {
        Some(found) if found.total == 0 => "No results".to_owned(),
        Some(found) => format!(
            "{}/{}",
            found.selected.map_or(0, |index| index + 1),
            found.total
        ),
        None => String::new(),
    };
    Some(
        row()
            .key("find")
            .pin(None, Some(12.0), Some(12.0), None)
            .gap(8.0)
            .items_center()
            .px(10.0)
            .py(6.0)
            .radius(6.0)
            .fill(theme.card)
            .border(1.0, theme.card_border)
            .child(
                text_input(&state.find.edit, tok::type_::BODY)
                    .key("find-input")
                    .label("Find")
                    .single_line()
                    .w(FIND_WIDTH)
                    .color(theme.text)
                    .on_edit(|state: &mut State| &mut state.find.edit)
                    .on_input_key(find_key),
            )
            .child(text(count).key("find-count").color(theme.text_secondary)),
    )
}

#[cfg(test)]
mod tests {
    use crate::test_support::*;
    use rune::{Key, Mods};

    #[test]
    fn command_f_opens_the_find_bar_and_escape_closes_it() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('f'), command());
        runtime.frame(2.0 / 60.0);
        assert!(runtime.find("find").is_some());
        press(&mut runtime, Key::Escape, Mods::default());
        runtime.frame(3.0 / 60.0);
        assert!(runtime.find("find").is_none());
        assert!(!runtime.state.find.open);
    }

    #[test]
    fn typing_goes_to_the_find_bar_not_the_terminal() {
        let mut runtime = workspace_runtime();
        press(&mut runtime, Key::Command('f'), command());
        runtime.frame(2.0 / 60.0);
        press(&mut runtime, Key::Text("ab".into()), Mods::default());
        assert_eq!(runtime.state.find.edit.text, "ab");
    }

    #[test]
    fn the_find_bar_closes_when_keys_move_to_another_tab() {
        let mut runtime = workspace_runtime();
        runtime.state.started = true;
        press(&mut runtime, Key::Command('f'), command());
        runtime.frame(2.0 / 60.0);
        press(&mut runtime, Key::Command('2'), command());
        runtime.frame(3.0 / 60.0);
        assert!(runtime.find("find").is_none());
    }
}
