use crate::actions::FormatSelections;
use crate::{
    actions::Format, selections_collection::SelectionsCollection, Copy, CopyPermalinkToLine, Cut,
    DisplayPoint, DisplaySnapshot, Editor, EditorMode, FindAllReferences, GoToDeclaration,
    GoToDefinition, GoToImplementation, GoToTypeDefinition, Paste, Rename, RevealInFileManager,
    SelectMode, ToDisplayPoint, ToggleCodeActions,
};
use gpui::prelude::FluentBuilder;
use gpui::{DismissEvent, Model, ModelContext, Pixels, Point, Subscription, Window};
use std::ops::Range;
use text::PointUtf16;
use workspace::OpenInTerminal;

#[derive(Debug)]
pub enum MenuPosition {
    /// When the editor is scrolled, the context menu stays on the exact
    /// same position on the screen, never disappearing.
    PinnedToScreen(Point<Pixels>),
    /// When the editor is scrolled, the context menu follows the position it is associated with.
    /// Disappears when the position is no longer visible.
    PinnedToEditor {
        source: multi_buffer::Anchor,
        offset: Point<Pixels>,
    },
}

pub struct MouseContextMenu {
    pub(crate) position: MenuPosition,
    pub(crate) context_menu: Model<ui::ContextMenu>,
    _subscription: Subscription,
}

impl std::fmt::Debug for MouseContextMenu {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MouseContextMenu")
            .field("position", &self.position)
            .field("context_menu", &self.context_menu)
            .finish()
    }
}

impl MouseContextMenu {
    pub(crate) fn pinned_to_editor(
        editor: &mut Editor,
        source: multi_buffer::Anchor,
        position: Point<Pixels>,
        context_menu: Model<ui::ContextMenu>,
        window: &mut Window,
        cx: &mut ModelContext<Editor>,
    ) -> Option<Self> {
        let editor_snapshot = editor.snapshot(window, cx);
        let content_origin = editor.last_bounds?.origin
            + Point {
                x: editor.gutter_dimensions.width,
                y: Pixels(0.0),
            };
        let source_position = editor.to_pixel_point(source, &editor_snapshot, window, cx)?;
        let menu_position = MenuPosition::PinnedToEditor {
            source,
            offset: position - (source_position + content_origin),
        };
        return Some(MouseContextMenu::new(
            menu_position,
            context_menu,
            window,
            cx,
        ));
    }

    pub(crate) fn new(
        position: MenuPosition,
        context_menu: Model<ui::ContextMenu>,
        window: &mut Window,
        cx: &mut ModelContext<Editor>,
    ) -> Self {
        let context_menu_focus = context_menu.focus_handle(cx);
        window.focus(&context_menu_focus);

        let _subscription = cx.subscribe_in(
            &context_menu,
            window,
            move |editor, _, _event: &DismissEvent, window, cx| {
                editor.mouse_context_menu.take();
                if context_menu_focus.contains_focused(window, cx) {
                    editor.focus(window, cx);
                }
            },
        );

        Self {
            position,
            context_menu,
            _subscription,
        }
    }
}

fn display_ranges<'a>(
    display_map: &'a DisplaySnapshot,
    selections: &'a SelectionsCollection,
) -> impl Iterator<Item = Range<DisplayPoint>> + 'a {
    let pending = selections
        .pending
        .as_ref()
        .map(|pending| &pending.selection);
    selections
        .disjoint
        .iter()
        .chain(pending)
        .map(move |s| s.start.to_display_point(display_map)..s.end.to_display_point(display_map))
}

pub fn deploy_context_menu(
    editor: &mut Editor,
    position: Option<Point<Pixels>>,
    point: DisplayPoint,
    window: &mut Window,
    cx: &mut ModelContext<Editor>,
) {
    if !editor.is_focused(window, cx) {
        editor.focus(window, cx);
    }

    // Don't show context menu for inline editors
    if editor.mode() != EditorMode::Full {
        return;
    }

    let display_map = editor.selections.display_map(cx);
    let source_anchor = display_map.display_point_to_anchor(point, text::Bias::Right);
    let context_menu = if let Some(custom) = editor.custom_context_menu.take() {
        let menu = custom(editor, point, cx);
        editor.custom_context_menu = Some(custom);
        let Some(menu) = menu else {
            return;
        };
        menu
    } else {
        // Don't show the context menu if there isn't a project associated with this editor
        if editor.project.is_none() {
            return;
        }

        let display_map = editor.selections.display_map(cx);
        let buffer = &editor.snapshot(window, cx).buffer_snapshot;
        let anchor = buffer.anchor_before(point.to_point(&display_map));
        if !display_ranges(&display_map, &editor.selections).any(|r| r.contains(&point)) {
            // Move the cursor to the clicked location so that dispatched actions make sense
            editor.change_selections(None, window, cx, |s| {
                s.clear_disjoint();
                s.set_pending_anchor_range(anchor..anchor, SelectMode::Character);
            });
        }

        let focus = window.focused(cx);
        let has_reveal_target = editor.target_file(cx).is_some();
        let reveal_in_finder_label = if cfg!(target_os = "macos") {
            "Reveal in Finder"
        } else {
            "Reveal in File Manager"
        };
        let has_selections = editor
            .selections
            .all::<PointUtf16>(cx)
            .into_iter()
            .any(|s| !s.is_empty());

        ui::ContextMenu::build(window, cx, |menu, _window, _cx| {
            let builder = menu
                .on_blur_subscription(Subscription::new(|| {}))
                .action("Go to Definition", Box::new(GoToDefinition))
                .action("Go to Declaration", Box::new(GoToDeclaration))
                .action("Go to Type Definition", Box::new(GoToTypeDefinition))
                .action("Go to Implementation", Box::new(GoToImplementation))
                .action("Find All References", Box::new(FindAllReferences))
                .separator()
                .action("Rename Symbol", Box::new(Rename))
                .action("Format Buffer", Box::new(Format))
                .when(has_selections, |cx| {
                    cx.action("Format Selections", Box::new(FormatSelections))
                })
                .action(
                    "Code Actions",
                    Box::new(ToggleCodeActions {
                        deployed_from_indicator: None,
                    }),
                )
                .separator()
                .action("Cut", Box::new(Cut))
                .action("Copy", Box::new(Copy))
                .action("Paste", Box::new(Paste))
                .separator()
                .map(|builder| {
                    if has_reveal_target {
                        builder.action(reveal_in_finder_label, Box::new(RevealInFileManager))
                    } else {
                        builder
                            .disabled_action(reveal_in_finder_label, Box::new(RevealInFileManager))
                    }
                })
                .action("Open in Terminal", Box::new(OpenInTerminal))
                .action("Copy Permalink", Box::new(CopyPermalinkToLine));
            match focus {
                Some(focus) => builder.context(focus),
                None => builder,
            }
        })
    };

    editor.mouse_context_menu = match position {
        Some(position) => MouseContextMenu::pinned_to_editor(
            editor,
            source_anchor,
            position,
            context_menu,
            window,
            cx,
        ),
        None => {
            let menu_position = MenuPosition::PinnedToEditor {
                source: source_anchor,
                offset: editor.character_size(window, cx),
            };
            Some(MouseContextMenu::new(
                menu_position,
                context_menu,
                window,
                cx,
            ))
        }
    };
    cx.notify();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{editor_tests::init_test, test::editor_lsp_test_context::EditorLspTestContext};
    use indoc::indoc;

    #[gpui::test]
    async fn test_mouse_context_menu(cx: &mut gpui::TestAppContext) {
        init_test(cx, |_| {});

        let mut cx = EditorLspTestContext::new_rust(
            lsp::ServerCapabilities {
                hover_provider: Some(lsp::HoverProviderCapability::Simple(true)),
                ..Default::default()
            },
            cx,
        )
        .await;

        cx.set_state(indoc! {"
            fn teˇst() {
                do_work();
            }
        "});
        let point = cx.display_point(indoc! {"
            fn test() {
                do_wˇork();
            }
        "});
        cx.editor(|editor, _window, _app| assert!(editor.mouse_context_menu.is_none()));
        cx.update_editor(|editor, window, cx| {
            deploy_context_menu(editor, Some(Default::default()), point, window, cx)
        });

        cx.assert_editor_state(indoc! {"
            fn test() {
                do_wˇork();
            }
        "});
        cx.editor(|editor, _window, _app| assert!(editor.mouse_context_menu.is_some()));
    }
}
