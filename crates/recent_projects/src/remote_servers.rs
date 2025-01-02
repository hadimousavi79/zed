use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::Arc;

use editor::Editor;
use file_finder::OpenPathDelegate;
use futures::channel::oneshot;
use futures::future::Shared;
use futures::FutureExt;
use gpui::canvas;
use gpui::ClipboardItem;
use gpui::Task;
use gpui::WeakView;
use gpui::{
    AnyElement, AppContext, DismissEvent, EventEmitter, FocusHandle, FocusableView, Model,
    ModelContext, PromptLevel, ScrollHandle, Window,
};
use picker::Picker;
use project::Project;
use remote::ssh_session::ConnectionIdentifier;
use remote::SshConnectionOptions;
use remote::SshRemoteClient;
use settings::update_settings_file;
use settings::Settings;
use ui::Navigable;
use ui::NavigableEntry;
use ui::{
    prelude::*, IconButtonShape, List, ListItem, ListSeparator, Modal, ModalHeader, Scrollbar,
    ScrollbarState, Section, Tooltip,
};
use util::ResultExt;
use workspace::notifications::NotificationId;
use workspace::OpenOptions;
use workspace::Toast;
use workspace::{notifications::DetachAndPromptErr, ModalView, Workspace};

use crate::ssh_connections::connect_over_ssh;
use crate::ssh_connections::open_ssh_project;
use crate::ssh_connections::RemoteSettingsContent;
use crate::ssh_connections::SshConnection;
use crate::ssh_connections::SshConnectionHeader;
use crate::ssh_connections::SshConnectionModal;
use crate::ssh_connections::SshProject;
use crate::ssh_connections::SshPrompt;
use crate::ssh_connections::SshSettings;
use crate::OpenRemote;

mod navigation_base {}
pub struct RemoteServerProjects {
    mode: Mode,
    focus_handle: FocusHandle,
    workspace: WeakView<Workspace>,
    retained_connections: Vec<Model<SshRemoteClient>>,
}

struct CreateRemoteServer {
    address_editor: Model<Editor>,
    address_error: Option<SharedString>,
    ssh_prompt: Option<Model<SshPrompt>>,
    _creating: Option<Task<Option<()>>>,
}

impl CreateRemoteServer {
    fn new(window: &mut Window, cx: &mut AppContext) -> Self {
        let address_editor = window.new_view(cx, Editor::single_line);
        address_editor.update(cx, |this, cx| {
            this.focus_handle(cx).focus(window);
        });
        Self {
            address_editor,
            address_error: None,
            ssh_prompt: None,
            _creating: None,
        }
    }
}

struct ProjectPicker {
    connection_string: SharedString,
    nickname: Option<SharedString>,
    picker: Model<Picker<OpenPathDelegate>>,
    _path_task: Shared<Task<Option<()>>>,
}

struct EditNicknameState {
    index: usize,
    editor: Model<Editor>,
}

impl EditNicknameState {
    fn new(index: usize, window: &mut Window, cx: &mut AppContext) -> Self {
        let this = Self {
            index,
            editor: window.new_view(cx, Editor::single_line),
        };
        let starting_text = SshSettings::get_global(cx)
            .ssh_connections()
            .nth(index)
            .and_then(|state| state.nickname.clone())
            .filter(|text| !text.is_empty());
        this.editor.update(cx, |this, cx| {
            this.set_placeholder_text("Add a nickname for this server", window, cx);
            if let Some(starting_text) = starting_text {
                this.set_text(starting_text, window, cx);
            }
        });
        this.editor.focus_handle(cx).focus(window);
        this
    }
}

impl FocusableView for ProjectPicker {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        self.picker.focus_handle(cx)
    }
}

impl ProjectPicker {
    fn new(
        ix: usize,
        connection: SshConnectionOptions,
        project: Model<Project>,
        workspace: WeakView<Workspace>,
        window: &mut Window,
        cx: &mut ModelContext<RemoteServerProjects>,
    ) -> Model<Self> {
        let (tx, rx) = oneshot::channel();
        let lister = project::DirectoryLister::Project(project.clone());
        let query = lister.default_query(cx);
        let delegate = file_finder::OpenPathDelegate::new(tx, lister);

        let picker = window.new_view(cx, |cx| {
            let picker = Picker::uniform_list(delegate, window, cx)
                .width(rems(34.))
                .modal(false);
            picker.set_query(query, window, cx);
            picker
        });
        let connection_string = connection.connection_string().into();
        let nickname = connection.nickname.clone().map(|nick| nick.into());
        let _path_task = cx
            .spawn_in(window, {
                let workspace = workspace.clone();
                move |this, mut cx| async move {
                    let Ok(Some(paths)) = rx.await else {
                        workspace
                            .update(&mut cx, |workspace, cx| {
                                let weak = cx.view().downgrade();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    RemoteServerProjects::new(window, cx, weak)
                                });
                            })
                            .log_err()?;
                        return None;
                    };

                    let app_state = workspace
                        .update(&mut cx, |workspace, _| workspace.app_state().clone())
                        .ok()?;
                    let options = cx
                        .update(|window, cx| (app_state.build_window_options)(None, cx))
                        .log_err()?;

                    cx.open_window(options, |cx| {
                        window.activate_window();

                        let fs = app_state.fs.clone();
                        update_settings_file::<SshSettings>(fs, cx, {
                            let paths = paths
                                .iter()
                                .map(|path| path.to_string_lossy().to_string())
                                .collect();
                            move |setting, _| {
                                if let Some(server) = setting
                                    .ssh_connections
                                    .as_mut()
                                    .and_then(|connections| connections.get_mut(ix))
                                {
                                    server.projects.insert(SshProject { paths });
                                }
                            }
                        });

                        let tasks = paths
                            .into_iter()
                            .map(|path| {
                                project.update(cx, |project, cx| {
                                    project.find_or_create_worktree(&path, true, cx)
                                })
                            })
                            .collect::<Vec<_>>();
                        window
                            .spawn(cx, |_| async move {
                                for task in tasks {
                                    task.await?;
                                }
                                Ok(())
                            })
                            .detach_and_prompt_err("Failed to open path", window, cx, |_, _, _| {
                                None
                            });

                        window.new_view(cx, |window, cx| {
                            let workspace = Workspace::new(
                                None,
                                project.clone(),
                                app_state.clone(),
                                window,
                                cx,
                            );

                            workspace
                                .client()
                                .telemetry()
                                .report_app_event("create ssh project".to_string());

                            workspace
                        })
                    })
                    .log_err();
                    this.update(&mut cx, |_, cx| {
                        cx.emit(DismissEvent);
                    })
                    .ok();
                    Some(())
                }
            })
            .shared();
        window.new_view(cx, |_| Self {
            _path_task,
            picker,
            connection_string,
            nickname,
        })
    }
}

impl gpui::Render for ProjectPicker {
    fn render(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        v_flex()
            .child(
                SshConnectionHeader {
                    connection_string: self.connection_string.clone(),
                    paths: Default::default(),
                    nickname: self.nickname.clone(),
                }
                .render(window, cx),
            )
            .child(
                div()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(self.picker.clone()),
            )
    }
}

#[derive(Clone)]
struct ProjectEntry {
    open_folder: NavigableEntry,
    projects: Vec<(NavigableEntry, SshProject)>,
    configure: NavigableEntry,
    connection: SshConnection,
}

#[derive(Clone)]
struct DefaultState {
    scrollbar: ScrollbarState,
    add_new_server: NavigableEntry,
    servers: Vec<ProjectEntry>,
}
impl DefaultState {
    fn new(window: &mut Window, cx: &mut AppContext) -> Self {
        let handle = ScrollHandle::new();
        let scrollbar = ScrollbarState::new(handle.clone());
        let add_new_server = NavigableEntry::new(&handle, window, cx);
        let servers = SshSettings::get_global(cx)
            .ssh_connections()
            .map(|connection| {
                let open_folder = NavigableEntry::new(&handle, window, cx);
                let configure = NavigableEntry::new(&handle, window, cx);
                let projects = connection
                    .projects
                    .iter()
                    .map(|project| (NavigableEntry::new(&handle, window, cx), project.clone()))
                    .collect();
                ProjectEntry {
                    open_folder,
                    configure,
                    projects,
                    connection,
                }
            })
            .collect();
        Self {
            scrollbar,
            add_new_server,
            servers,
        }
    }
}

#[derive(Clone)]
struct ViewServerOptionsState {
    server_index: usize,
    connection: SshConnection,
    entries: [NavigableEntry; 4],
}
enum Mode {
    Default(DefaultState),
    ViewServerOptions(ViewServerOptionsState),
    EditNickname(EditNicknameState),
    ProjectPicker(Model<ProjectPicker>),
    CreateRemoteServer(CreateRemoteServer),
}

impl Mode {
    fn default_mode(window: &mut Window, cx: &mut AppContext) -> Self {
        Self::Default(DefaultState::new(window, cx))
    }
}
impl RemoteServerProjects {
    pub fn register(
        workspace: &mut Workspace,
        _window: &mut Window,
        _: &mut ModelContext<Workspace>,
    ) {
        workspace.register_action(|workspace, _: &OpenRemote, window, cx| {
            let handle = cx.view().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| Self::new(window, cx, handle))
        });
    }

    pub fn open(workspace: Model<Workspace>, window: &mut Window, cx: &mut AppContext) {
        workspace.update(cx, |workspace, cx| {
            let handle = cx.view().downgrade();
            workspace.toggle_modal(window, cx, |window, cx| Self::new(window, cx, handle))
        })
    }

    pub fn new(
        window: &mut Window,
        cx: &mut ModelContext<Self>,
        workspace: WeakView<Workspace>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let mut base_style = window.text_style();
        base_style.refine(&gpui::TextStyleRefinement {
            color: Some(cx.theme().colors().editor_foreground),
            ..Default::default()
        });

        Self {
            mode: Mode::default_mode(window, cx),
            focus_handle,
            workspace,
            retained_connections: Vec::new(),
        }
    }

    pub fn project_picker(
        ix: usize,
        connection_options: remote::SshConnectionOptions,
        project: Model<Project>,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
        workspace: WeakView<Workspace>,
    ) -> Self {
        let mut this = Self::new(window, cx, workspace.clone());
        this.mode = Mode::ProjectPicker(ProjectPicker::new(
            ix,
            connection_options,
            project,
            workspace,
            window,
            cx,
        ));
        cx.notify();

        this
    }

    fn create_ssh_server(
        &mut self,
        editor: Model<Editor>,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let input = get_text(&editor, window, cx);
        if input.is_empty() {
            return;
        }

        let connection_options = match SshConnectionOptions::parse_command_line(&input) {
            Ok(c) => c,
            Err(e) => {
                self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                    address_editor: editor,
                    address_error: Some(format!("could not parse: {:?}", e).into()),
                    ssh_prompt: None,
                    _creating: None,
                });
                return;
            }
        };
        let ssh_prompt = window.new_view(cx, |cx| SshPrompt::new(&connection_options, window, cx));

        let connection = connect_over_ssh(
            ConnectionIdentifier::setup(),
            connection_options.clone(),
            ssh_prompt.clone(),
            window,
            cx,
        )
        .prompt_err("Failed to connect", window, cx, |_, _, _| None);

        let address_editor = editor.clone();
        let creating = cx.spawn_in(window, move |this, mut cx| async move {
            match connection.await {
                Some(Some(client)) => this
                    .update(&mut cx, |this, cx| {
                        let _ = this.workspace.update(cx, |workspace, _| {
                            workspace
                                .client()
                                .telemetry()
                                .report_app_event("create ssh server".to_string())
                        });
                        this.retained_connections.push(client);
                        this.add_ssh_server(connection_options, window, cx);
                        this.mode = Mode::default_mode(window, cx);
                        cx.notify()
                    })
                    .log_err(),
                _ => this
                    .update(&mut cx, |this, cx| {
                        address_editor.update(cx, |this, _| {
                            this.set_read_only(false);
                        });
                        this.mode = Mode::CreateRemoteServer(CreateRemoteServer {
                            address_editor,
                            address_error: None,
                            ssh_prompt: None,
                            _creating: None,
                        });
                        cx.notify()
                    })
                    .log_err(),
            };
            None
        });

        editor.update(cx, |this, _| {
            this.set_read_only(true);
        });
        self.mode = Mode::CreateRemoteServer(CreateRemoteServer {
            address_editor: editor,
            address_error: None,
            ssh_prompt: Some(ssh_prompt.clone()),
            _creating: Some(creating),
        });
    }

    fn view_server_options(
        &mut self,
        (server_index, connection): (usize, SshConnection),
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        self.mode = Mode::ViewServerOptions(ViewServerOptionsState {
            server_index,
            connection,
            entries: std::array::from_fn(|_| NavigableEntry::focusable(window, cx)),
        });
        self.focus_handle(cx).focus(window);
        cx.notify();
    }

    fn create_ssh_project(
        &mut self,
        ix: usize,
        ssh_connection: SshConnection,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let Some(workspace) = self.workspace.upgrade() else {
            return;
        };

        let connection_options = ssh_connection.into();
        workspace.update(cx, |_, cx| {
            cx.defer_in(window, move |workspace, window, cx| {
                workspace.toggle_modal(window, cx, |window, cx| {
                    SshConnectionModal::new(&connection_options, Vec::new(), window, cx)
                });
                let prompt = workspace
                    .active_modal::<SshConnectionModal>(cx)
                    .unwrap()
                    .read(cx)
                    .prompt
                    .clone();

                let connect = connect_over_ssh(
                    ConnectionIdentifier::setup(),
                    connection_options.clone(),
                    prompt,
                    window,
                    cx,
                )
                .prompt_err("Failed to connect", window, cx, |_, _, _| None);

                cx.spawn_in(window, move |workspace, mut cx| async move {
                    let session = connect.await;

                    workspace
                        .update(&mut cx, |workspace, cx| {
                            if let Some(prompt) = workspace.active_modal::<SshConnectionModal>(cx) {
                                prompt.update(cx, |prompt, cx| prompt.finished(window, cx))
                            }
                        })
                        .ok();

                    let Some(Some(session)) = session else {
                        workspace
                            .update(&mut cx, |workspace, cx| {
                                let weak = cx.view().downgrade();
                                workspace.toggle_modal(window, cx, |window, cx| {
                                    RemoteServerProjects::new(window, cx, weak)
                                });
                            })
                            .log_err();
                        return;
                    };

                    workspace
                        .update(&mut cx, |workspace, cx| {
                            let app_state = workspace.app_state().clone();
                            let weak = cx.view().downgrade();
                            let project = project::Project::ssh(
                                session,
                                app_state.client.clone(),
                                app_state.node_runtime.clone(),
                                app_state.user_store.clone(),
                                app_state.languages.clone(),
                                app_state.fs.clone(),
                                cx,
                            );
                            workspace.toggle_modal(window, cx, |window, cx| {
                                RemoteServerProjects::project_picker(
                                    ix,
                                    connection_options,
                                    project,
                                    window,
                                    cx,
                                    weak,
                                )
                            });
                        })
                        .ok();
                })
                .detach()
            })
        })
    }

    fn confirm(&mut self, _: &menu::Confirm, window: &mut Window, cx: &mut ModelContext<Self>) {
        match &self.mode {
            Mode::Default(_) | Mode::ViewServerOptions(_) => {}
            Mode::ProjectPicker(_) => {}
            Mode::CreateRemoteServer(state) => {
                if let Some(prompt) = state.ssh_prompt.as_ref() {
                    prompt.update(cx, |prompt, cx| {
                        prompt.confirm(window, cx);
                    });
                    return;
                }

                self.create_ssh_server(state.address_editor.clone(), window, cx);
            }
            Mode::EditNickname(state) => {
                let text = Some(state.editor.read(cx).text(cx)).filter(|text| !text.is_empty());
                let index = state.index;
                self.update_settings_file(window, cx, move |setting, _| {
                    if let Some(connections) = setting.ssh_connections.as_mut() {
                        if let Some(connection) = connections.get_mut(index) {
                            connection.nickname = text;
                        }
                    }
                });
                self.mode = Mode::default_mode(window, cx);
                self.focus_handle.focus(window);
            }
        }
    }

    fn cancel(&mut self, _: &menu::Cancel, window: &mut Window, cx: &mut ModelContext<Self>) {
        match &self.mode {
            Mode::Default(_) => cx.emit(DismissEvent),
            Mode::CreateRemoteServer(state) if state.ssh_prompt.is_some() => {
                let new_state = CreateRemoteServer::new(window, cx);
                let old_prompt = state.address_editor.read(cx).text(cx);
                new_state.address_editor.update(cx, |this, cx| {
                    this.set_text(old_prompt, window, cx);
                });

                self.mode = Mode::CreateRemoteServer(new_state);
                cx.notify();
            }
            _ => {
                self.mode = Mode::default_mode(window, cx);
                self.focus_handle(cx).focus(window);
                cx.notify();
            }
        }
    }

    fn render_ssh_connection(
        &mut self,
        ix: usize,
        ssh_server: ProjectEntry,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        let (main_label, aux_label) = if let Some(nickname) = ssh_server.connection.nickname.clone()
        {
            let aux_label = SharedString::from(format!("({})", ssh_server.connection.host));
            (nickname.into(), Some(aux_label))
        } else {
            (ssh_server.connection.host.clone(), None)
        };
        v_flex()
            .w_full()
            .child(ListSeparator)
            .child(
                h_flex()
                    .group("ssh-server")
                    .w_full()
                    .pt_0p5()
                    .px_3()
                    .gap_1()
                    .overflow_hidden()
                    .child(
                        div().max_w_96().overflow_hidden().text_ellipsis().child(
                            Label::new(main_label)
                                .size(LabelSize::Small)
                                .color(Color::Muted),
                        ),
                    )
                    .children(
                        aux_label.map(|label| {
                            Label::new(label).size(LabelSize::Small).color(Color::Muted)
                        }),
                    ),
            )
            .child(
                List::new()
                    .empty_message("No projects.")
                    .children(ssh_server.projects.iter().enumerate().map(|(pix, p)| {
                        v_flex().gap_0p5().child(self.render_ssh_project(
                            ix,
                            &ssh_server,
                            pix,
                            p,
                            window,
                            cx,
                        ))
                    }))
                    .child(
                        h_flex()
                            .id(("new-remote-project-container", ix))
                            .track_focus(&ssh_server.open_folder.focus_handle)
                            .anchor_scroll(ssh_server.open_folder.scroll_anchor.clone())
                            .on_action(cx.listener({
                                let ssh_connection = ssh_server.clone();
                                move |this, _: &menu::Confirm, cx| {
                                    this.create_ssh_project(
                                        ix,
                                        ssh_connection.connection.clone(),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(
                                ListItem::new(("new-remote-project", ix))
                                    .toggle_state(
                                        ssh_server
                                            .open_folder
                                            .focus_handle
                                            .contains_focused(window, cx),
                                    )
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                                    .child(Label::new("Open Folder"))
                                    .on_click(cx.listener({
                                        let ssh_connection = ssh_server.clone();
                                        move |this, _, cx| {
                                            this.create_ssh_project(
                                                ix,
                                                ssh_connection.connection.clone(),
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    )
                    .child(
                        h_flex()
                            .id(("server-options-container", ix))
                            .track_focus(&ssh_server.configure.focus_handle)
                            .anchor_scroll(ssh_server.configure.scroll_anchor.clone())
                            .on_action(cx.listener({
                                let ssh_connection = ssh_server.clone();
                                move |this, _: &menu::Confirm, cx| {
                                    this.view_server_options(
                                        (ix, ssh_connection.connection.clone()),
                                        window,
                                        cx,
                                    );
                                }
                            }))
                            .child(
                                ListItem::new(("server-options", ix))
                                    .toggle_state(
                                        ssh_server
                                            .configure
                                            .focus_handle
                                            .contains_focused(window, cx),
                                    )
                                    .inset(true)
                                    .spacing(ui::ListItemSpacing::Sparse)
                                    .start_slot(Icon::new(IconName::Settings).color(Color::Muted))
                                    .child(Label::new("View Server Options"))
                                    .on_click(cx.listener({
                                        let ssh_connection = ssh_server.clone();
                                        move |this, _, cx| {
                                            this.view_server_options(
                                                (ix, ssh_connection.connection.clone()),
                                                window,
                                                cx,
                                            );
                                        }
                                    })),
                            ),
                    ),
            )
    }

    fn render_ssh_project(
        &mut self,
        server_ix: usize,
        server: &ProjectEntry,
        ix: usize,
        (navigation, project): &(NavigableEntry, SshProject),
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        let server = server.clone();
        let element_id_base = SharedString::from(format!("remote-project-{server_ix}"));
        let container_element_id_base =
            SharedString::from(format!("remote-project-container-{element_id_base}"));

        let callback = Arc::new({
            let project = project.clone();
            move |this: &mut Self, window: &mut Window, cx: &mut ModelContext<Self>| {
                let Some(app_state) = this
                    .workspace
                    .update(cx, |workspace, _| workspace.app_state().clone())
                    .log_err()
                else {
                    return;
                };
                let project = project.clone();
                let server = server.connection.clone();
                cx.emit(DismissEvent);
                cx.spawn_in(window, |_, mut cx| async move {
                    let result = open_ssh_project(
                        server.into(),
                        project.paths.into_iter().map(PathBuf::from).collect(),
                        app_state,
                        OpenOptions::default(),
                        &mut cx,
                    )
                    .await;
                    if let Err(e) = result {
                        log::error!("Failed to connect: {:?}", e);
                        cx.prompt(
                            gpui::PromptLevel::Critical,
                            "Failed to connect",
                            Some(&e.to_string()),
                            &["Ok"],
                        )
                        .await
                        .ok();
                    }
                })
                .detach();
            }
        });

        div()
            .id((container_element_id_base, ix))
            .track_focus(&navigation.focus_handle)
            .anchor_scroll(navigation.scroll_anchor.clone())
            .on_action(cx.listener({
                let callback = callback.clone();
                move |this, _: &menu::Confirm, cx| {
                    callback(this, cx);
                }
            }))
            .child(
                ListItem::new((element_id_base, ix))
                    .toggle_state(navigation.focus_handle.contains_focused(window, cx))
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(
                        Icon::new(IconName::Folder)
                            .color(Color::Muted)
                            .size(IconSize::Small),
                    )
                    .child(Label::new(project.paths.join(", ")))
                    .on_click(cx.listener(move |this, _, window, cx| callback(this, cx)))
                    .end_hover_slot::<AnyElement>(Some(
                        div()
                            .mr_2()
                            .child({
                                let project = project.clone();
                                // Right-margin to offset it from the Scrollbar
                                IconButton::new("remove-remote-project", IconName::TrashAlt)
                                    .icon_size(IconSize::Small)
                                    .shape(IconButtonShape::Square)
                                    .size(ButtonSize::Large)
                                    .tooltip(|window, cx| {
                                        Tooltip::text("Delete Remote Project", window, cx)
                                    })
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.delete_ssh_project(server_ix, &project, window, cx)
                                    }))
                            })
                            .into_any_element(),
                    )),
            )
    }

    fn update_settings_file(
        &mut self,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
        f: impl FnOnce(&mut RemoteSettingsContent, &AppContext) + Send + Sync + 'static,
    ) {
        let Some(fs) = self
            .workspace
            .update(cx, |workspace, _| workspace.app_state().fs.clone())
            .log_err()
        else {
            return;
        };
        update_settings_file::<SshSettings>(fs, cx, move |setting, cx| f(setting, cx));
    }

    fn delete_ssh_server(
        &mut self,
        server: usize,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        self.update_settings_file(window, cx, move |setting, _| {
            if let Some(connections) = setting.ssh_connections.as_mut() {
                connections.remove(server);
            }
        });
    }

    fn delete_ssh_project(
        &mut self,
        server: usize,
        project: &SshProject,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        let project = project.clone();
        self.update_settings_file(window, cx, move |setting, _| {
            if let Some(server) = setting
                .ssh_connections
                .as_mut()
                .and_then(|connections| connections.get_mut(server))
            {
                server.projects.remove(&project);
            }
        });
    }

    fn add_ssh_server(
        &mut self,
        connection_options: remote::SshConnectionOptions,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) {
        self.update_settings_file(window, cx, move |setting, _| {
            setting
                .ssh_connections
                .get_or_insert(Default::default())
                .push(SshConnection {
                    host: SharedString::from(connection_options.host),
                    username: connection_options.username,
                    port: connection_options.port,
                    projects: BTreeSet::<SshProject>::new(),
                    nickname: None,
                    args: connection_options.args.unwrap_or_default(),
                    upload_binary_over_ssh: None,
                })
        });
    }

    fn render_create_remote_server(
        &self,
        state: &CreateRemoteServer,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        let ssh_prompt = state.ssh_prompt.clone();

        state.address_editor.update(cx, |editor, cx| {
            if editor.text(cx).is_empty() {
                editor.set_placeholder_text("ssh user@example -p 2222", window, cx);
            }
        });

        let theme = cx.theme();

        v_flex()
            .track_focus(&self.focus_handle(cx))
            .id("create-remote-server")
            .overflow_hidden()
            .size_full()
            .flex_1()
            .child(
                div()
                    .p_2()
                    .border_b_1()
                    .border_color(theme.colors().border_variant)
                    .child(state.address_editor.clone()),
            )
            .child(
                h_flex()
                    .bg(theme.colors().editor_background)
                    .rounded_b_md()
                    .w_full()
                    .map(|this| {
                        if let Some(ssh_prompt) = ssh_prompt {
                            this.child(h_flex().w_full().child(ssh_prompt))
                        } else if let Some(address_error) = &state.address_error {
                            this.child(
                                h_flex().p_2().w_full().gap_2().child(
                                    Label::new(address_error.clone())
                                        .size(LabelSize::Small)
                                        .color(Color::Error),
                                ),
                            )
                        } else {
                            this.child(
                                h_flex()
                                    .p_2()
                                    .w_full()
                                    .gap_1()
                                    .child(
                                        Label::new(
                                            "Enter the command you use to SSH into this server.",
                                        )
                                        .color(Color::Muted)
                                        .size(LabelSize::Small),
                                    )
                                    .child(
                                        Button::new("learn-more", "Learn more…")
                                            .label_size(LabelSize::Small)
                                            .size(ButtonSize::None)
                                            .color(Color::Accent)
                                            .style(ButtonStyle::Transparent)
                                            .on_click(|_, window, cx| {
                                                cx.open_url(
                                                    "https://zed.dev/docs/remote-development",
                                                );
                                            }),
                                    ),
                            )
                        }
                    }),
            )
    }

    fn render_view_options(
        &mut self,
        ViewServerOptionsState {
            server_index,
            connection,
            entries,
        }: ViewServerOptionsState,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        let connection_string = connection.host.clone();

        let mut view = Navigable::new(
            div()
                .track_focus(&self.focus_handle(cx))
                .size_full()
                .child(
                    SshConnectionHeader {
                        connection_string: connection_string.clone(),
                        paths: Default::default(),
                        nickname: connection.nickname.clone().map(|s| s.into()),
                    }
                    .render(window, cx),
                )
                .child(
                    v_flex()
                        .pb_1()
                        .child(ListSeparator)
                        .child({
                            let label = if connection.nickname.is_some() {
                                "Edit Nickname"
                            } else {
                                "Add Nickname to Server"
                            };
                            div()
                                .id("ssh-options-add-nickname")
                                .track_focus(&entries[0].focus_handle)
                                .on_action(cx.listener(
                                    move |this, _: &menu::Confirm, window, cx| {
                                        this.mode = Mode::EditNickname(EditNicknameState::new(
                                            server_index,
                                            window,
                                            cx,
                                        ));
                                        cx.notify();
                                    },
                                ))
                                .child(
                                    ListItem::new("add-nickname")
                                        .toggle_state(
                                            entries[0].focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(Icon::new(IconName::Pencil).color(Color::Muted))
                                        .child(Label::new(label))
                                        .on_click(cx.listener(move |this, _, window, cx| {
                                            this.mode = Mode::EditNickname(EditNicknameState::new(
                                                server_index,
                                                window,
                                                cx,
                                            ));
                                            cx.notify();
                                        })),
                                )
                        })
                        .child({
                            let workspace = self.workspace.clone();
                            fn callback(
                                workspace: WeakView<Workspace>,
                                connection_string: SharedString,
                                window: &mut Window,
                                cx: &mut AppContext,
                            ) {
                                cx.write_to_clipboard(ClipboardItem::new_string(
                                    connection_string.to_string(),
                                ));
                                workspace
                                    .update(cx, |this, cx| {
                                        struct SshServerAddressCopiedToClipboard;
                                        let notification = format!(
                                            "Copied server address ({}) to clipboard",
                                            connection_string
                                        );

                                        this.show_toast(
                                            Toast::new(
                                                NotificationId::composite::<
                                                    SshServerAddressCopiedToClipboard,
                                                >(
                                                    connection_string.clone()
                                                ),
                                                notification,
                                            )
                                            .autohide(),
                                            window,
                                            cx,
                                        );
                                    })
                                    .ok();
                            }
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&entries[1].focus_handle)
                                .on_action({
                                    let connection_string = connection_string.clone();
                                    let workspace = self.workspace.clone();
                                    move |_: &menu::Confirm, cx| {
                                        callback(
                                            workspace.clone(),
                                            connection_string.clone(),
                                            window,
                                            cx,
                                        );
                                    }
                                })
                                .child(
                                    ListItem::new("copy-server-address")
                                        .toggle_state(
                                            entries[1].focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(Icon::new(IconName::Copy).color(Color::Muted))
                                        .child(Label::new("Copy Server Address"))
                                        .end_hover_slot(
                                            Label::new(connection_string.clone())
                                                .color(Color::Muted),
                                        )
                                        .on_click({
                                            let connection_string = connection_string.clone();
                                            move |_, cx| {
                                                callback(
                                                    workspace.clone(),
                                                    connection_string.clone(),
                                                    window,
                                                    cx,
                                                );
                                            }
                                        }),
                                )
                        })
                        .child({
                            fn remove_ssh_server(
                                remote_servers: Model<RemoteServerProjects>,
                                index: usize,
                                connection_string: SharedString,
                                window: &mut Window,
                                cx: &mut AppContext,
                            ) {
                                let prompt_message =
                                    format!("Remove server `{}`?", connection_string);

                                let confirmation = window.prompt(
                                    PromptLevel::Warning,
                                    &prompt_message,
                                    None,
                                    &["Yes, remove it", "No, keep it"],
                                    cx,
                                );

                                window
                                    .spawn(cx, |mut cx| async move {
                                        if confirmation.await.ok() == Some(0) {
                                            remote_servers
                                                .update(&mut cx, |this, cx| {
                                                    this.delete_ssh_server(index, window, cx);
                                                })
                                                .ok();
                                            remote_servers
                                                .update(&mut cx, |this, cx| {
                                                    this.mode = Mode::default_mode(window, cx);
                                                    cx.notify();
                                                })
                                                .ok();
                                        }
                                        anyhow::Ok(())
                                    })
                                    .detach_and_log_err(cx);
                            }
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&entries[2].focus_handle)
                                .on_action(cx.listener({
                                    let connection_string = connection_string.clone();
                                    move |_, _: &menu::Confirm, cx| {
                                        remove_ssh_server(
                                            cx.view().clone(),
                                            server_index,
                                            connection_string.clone(),
                                            window,
                                            cx,
                                        );
                                        cx.focus_self(window);
                                    }
                                }))
                                .child(
                                    ListItem::new("remove-server")
                                        .toggle_state(
                                            entries[2].focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(Icon::new(IconName::Trash).color(Color::Error))
                                        .child(Label::new("Remove Server").color(Color::Error))
                                        .on_click(cx.listener(move |_, _, window, cx| {
                                            remove_ssh_server(
                                                cx.view().clone(),
                                                server_index,
                                                connection_string.clone(),
                                                window,
                                                cx,
                                            );
                                            cx.focus_self(window);
                                        })),
                                )
                        })
                        .child(ListSeparator)
                        .child({
                            div()
                                .id("ssh-options-copy-server-address")
                                .track_focus(&entries[3].focus_handle)
                                .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                                    this.mode = Mode::default_mode(window, cx);
                                    cx.focus_self(window);
                                    cx.notify();
                                }))
                                .child(
                                    ListItem::new("go-back")
                                        .toggle_state(
                                            entries[3].focus_handle.contains_focused(window, cx),
                                        )
                                        .inset(true)
                                        .spacing(ui::ListItemSpacing::Sparse)
                                        .start_slot(
                                            Icon::new(IconName::ArrowLeft).color(Color::Muted),
                                        )
                                        .child(Label::new("Go Back"))
                                        .on_click(cx.listener(|this, _, window, cx| {
                                            this.mode = Mode::default_mode(window, cx);
                                            cx.focus_self(window);
                                            cx.notify()
                                        })),
                                )
                        }),
                )
                .into_any_element(),
        );
        for entry in entries {
            view = view.entry(entry);
        }

        view.render(window, cx).into_any_element()
    }

    fn render_edit_nickname(
        &self,
        state: &EditNicknameState,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        let Some(connection) = SshSettings::get_global(cx)
            .ssh_connections()
            .nth(state.index)
        else {
            return v_flex()
                .id("ssh-edit-nickname")
                .track_focus(&self.focus_handle(cx));
        };

        let connection_string = connection.host.clone();
        let nickname = connection.nickname.clone().map(|s| s.into());

        v_flex()
            .id("ssh-edit-nickname")
            .track_focus(&self.focus_handle(cx))
            .child(
                SshConnectionHeader {
                    connection_string,
                    paths: Default::default(),
                    nickname,
                }
                .render(window, cx),
            )
            .child(
                h_flex()
                    .p_2()
                    .border_t_1()
                    .border_color(cx.theme().colors().border_variant)
                    .child(state.editor.clone()),
            )
    }

    fn render_default(
        &mut self,
        mut state: DefaultState,
        window: &mut Window,
        cx: &mut ModelContext<Self>,
    ) -> impl IntoElement {
        if SshSettings::get_global(cx)
            .ssh_connections
            .as_ref()
            .map_or(false, |connections| {
                state
                    .servers
                    .iter()
                    .map(|server| &server.connection)
                    .ne(connections.iter())
            })
        {
            self.mode = Mode::default_mode(window, cx);
            if let Mode::Default(new_state) = &self.mode {
                state = new_state.clone();
            }
        }
        let scroll_state = state.scrollbar.parent_view(cx.view());
        let connect_button = div()
            .id("ssh-connect-new-server-container")
            .track_focus(&state.add_new_server.focus_handle)
            .anchor_scroll(state.add_new_server.scroll_anchor.clone())
            .child(
                ListItem::new("register-remove-server-button")
                    .toggle_state(
                        state
                            .add_new_server
                            .focus_handle
                            .contains_focused(window, cx),
                    )
                    .inset(true)
                    .spacing(ui::ListItemSpacing::Sparse)
                    .start_slot(Icon::new(IconName::Plus).color(Color::Muted))
                    .child(Label::new("Connect New Server"))
                    .on_click(cx.listener(|this, _, window, cx| {
                        let state = CreateRemoteServer::new(window, cx);
                        this.mode = Mode::CreateRemoteServer(state);

                        cx.notify();
                    })),
            )
            .on_action(cx.listener(|this, _: &menu::Confirm, window, cx| {
                let state = CreateRemoteServer::new(window, cx);
                this.mode = Mode::CreateRemoteServer(state);

                cx.notify();
            }));

        let ui::ScrollableHandle::NonUniform(scroll_handle) = scroll_state.scroll_handle() else {
            unreachable!()
        };

        let mut modal_section = Navigable::new(
            v_flex()
                .track_focus(&self.focus_handle(cx))
                .id("ssh-server-list")
                .overflow_y_scroll()
                .track_scroll(&scroll_handle)
                .size_full()
                .child(connect_button)
                .child(
                    List::new()
                        .empty_message(
                            v_flex()
                                .child(
                                    div().px_3().child(
                                        Label::new("No remote servers registered yet.")
                                            .color(Color::Muted),
                                    ),
                                )
                                .into_any_element(),
                        )
                        .children(state.servers.iter().enumerate().map(|(ix, connection)| {
                            self.render_ssh_connection(ix, connection.clone(), window, cx)
                                .into_any_element()
                        })),
                )
                .into_any_element(),
        )
        .entry(state.add_new_server.clone());

        for server in &state.servers {
            for (navigation_state, _) in &server.projects {
                modal_section = modal_section.entry(navigation_state.clone());
            }
            modal_section = modal_section
                .entry(server.open_folder.clone())
                .entry(server.configure.clone());
        }
        let mut modal_section = modal_section.render(window, cx).into_any_element();

        Modal::new("remote-projects", None)
            .header(
                ModalHeader::new()
                    .child(Headline::new("Remote Projects (beta)").size(HeadlineSize::XSmall)),
            )
            .section(
                Section::new().padded(false).child(
                    v_flex()
                        .min_h(rems(20.))
                        .size_full()
                        .relative()
                        .child(ListSeparator)
                        .child(
                            canvas(
                                |bounds, window, cx| {
                                    modal_section.prepaint_as_root(
                                        bounds.origin,
                                        bounds.size.into(),
                                        cx,
                                    );
                                    modal_section
                                },
                                |_, mut modal_section, window, cx| {
                                    modal_section.paint(cx);
                                },
                            )
                            .size_full(),
                        )
                        .child(
                            div()
                                .occlude()
                                .h_full()
                                .absolute()
                                .top_1()
                                .bottom_1()
                                .right_1()
                                .w(px(8.))
                                .children(Scrollbar::vertical(scroll_state)),
                        ),
                ),
            )
            .into_any_element()
    }
}

fn get_text(element: &Model<Editor>, window: &mut Window, cx: &mut AppContext) -> String {
    element.read(cx).text(cx).trim().to_string()
}

impl ModalView for RemoteServerProjects {}

impl FocusableView for RemoteServerProjects {
    fn focus_handle(&self, cx: &AppContext) -> FocusHandle {
        match &self.mode {
            Mode::ProjectPicker(picker) => picker.focus_handle(cx),
            _ => self.focus_handle.clone(),
        }
    }
}

impl EventEmitter<DismissEvent> for RemoteServerProjects {}

impl Render for RemoteServerProjects {
    fn render(&mut self, window: &mut Window, cx: &mut ModelContext<Self>) -> impl IntoElement {
        div()
            .elevation_3(window, cx)
            .w(rems(34.))
            .key_context("RemoteServerModal")
            .on_action(cx.listener(Self::cancel))
            .on_action(cx.listener(Self::confirm))
            .capture_any_mouse_down(cx.listener(|this, _, window, cx| {
                this.focus_handle(cx).focus(window);
            }))
            .on_mouse_down_out(cx.listener(|this, _, window, cx| {
                if matches!(this.mode, Mode::Default(_)) {
                    cx.emit(DismissEvent)
                }
            }))
            .child(match &self.mode {
                Mode::Default(state) => self
                    .render_default(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ViewServerOptions(state) => self
                    .render_view_options(state.clone(), window, cx)
                    .into_any_element(),
                Mode::ProjectPicker(element) => element.clone().into_any_element(),
                Mode::CreateRemoteServer(state) => self
                    .render_create_remote_server(state, window, cx)
                    .into_any_element(),
                Mode::EditNickname(state) => self
                    .render_edit_nickname(state, window, cx)
                    .into_any_element(),
            })
    }
}
