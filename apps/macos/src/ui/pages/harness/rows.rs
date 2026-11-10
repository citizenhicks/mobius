//! The option menu a Bot's settings rows render with.

use super::*;
use std::rc::Rc;

/// One menu choice: its value (`None` = unset) and label.
pub(in crate::ui) type Choice = (Option<Arc<str>>, SharedString);

/// A menu of `choices` showing `current`; picking one calls `pick` with its value.
pub(in crate::ui) fn option_menu(
    id: impl Into<ElementId>,
    choices: Vec<Choice>,
    current: Option<&str>,
    pick: impl Fn(&mut Workspace, Option<Arc<str>>, &mut Context<Workspace>) + 'static,
    cx: &Context<Workspace>,
) -> AnyElement {
    let shown = choices
        .iter()
        .find(|(value, _)| value.as_deref() == current)
        .map_or_else(
            || SharedString::new_static("Choose…"),
            |(_, label)| SharedString::clone(label),
        );
    let pick = Rc::new(pick);
    let items = choices
        .into_iter()
        .map(|(value, label)| {
            let checked = value.as_deref() == current;
            let pick = Rc::clone(&pick);
            MenuChoice::new(
                label,
                checked,
                cx.listener(move |this, _: &ClickEvent, _, cx| {
                    pick(this, value.as_ref().map(Arc::clone), cx);
                }),
            )
        })
        .collect();
    menu_select(id, shown, None, items, cx).into_any_element()
}
