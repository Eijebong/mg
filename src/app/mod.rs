/*
 * Copyright (c) 2016 Boucher, Antoni <bouanto@zoho.com>
 *
 * Permission is hereby granted, free of charge, to any person obtaining a copy of
 * this software and associated documentation files (the "Software"), to deal in
 * the Software without restriction, including without limitation the rights to
 * use, copy, modify, merge, publish, distribute, sublicense, and/or sell copies of
 * the Software, and to permit persons to whom the Software is furnished to do so,
 * subject to the following conditions:
 *
 * The above copyright notice and this permission notice shall be included in all
 * copies or substantial portions of the Software.
 *
 * THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
 * IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY, FITNESS
 * FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE AUTHORS OR
 * COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER LIABILITY, WHETHER
 * IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM, OUT OF OR IN
 * CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE SOFTWARE.
 */

pub mod dialog;
mod message;
pub mod settings;
mod shortcut;
pub mod status_bar;

use std::borrow::Cow;
use std::char;
use std::collections::HashMap;
use std::fs::File;
use std::io::BufReader;
use std::path::{Path, PathBuf};

use gdk::{EventKey, RGBA};
use gdk::enums::key::{Escape, colon};
use gdk_sys::GdkRGBA;
use gtk::{
    self,
    ContainerExt,
    EditableSignals,
    Grid,
    Inhibit,
    IsA,
    Overlay,
    Settings,
    Widget,
    WidgetExt,
    Window,
    WindowExt,
    WindowType,
    STATE_FLAG_NORMAL,
};
use gtk::prelude::WidgetExtManual;
use mg_settings::{self, Config, EnumFromStr, EnumMetaData, Parser, SettingCompletion};
use mg_settings::Command::{self, App, Custom, Map, Set, Unmap};
use mg_settings::error::{Error, Result};
use mg_settings::error::ErrorType::{MissingArgument, NoCommand, Parse, UnknownCommand};
use mg_settings::key::Key;

use completion::{
    CommandCompleter,
    Completer,
    CompletionView,
    SettingCompleter,
    DEFAULT_COMPLETER_IDENT,
    NO_COMPLETER_IDENT,
};
use gobject::ObjectExtManual;
use self::ActivationType::{Current, Final};
use self::settings::NoSettings;
use self::ShortcutCommand::{Complete, Incomplete};
use self::status_bar::StatusBar;
pub use self::status_bar::StatusBarItem;
use style_context::StyleContextExtManual;
use super::{NoSpecialCommands, SpecialCommand};

#[derive(PartialEq)]
enum ActivationType {
    Current,
    Final,
}

type Modes = HashMap<String, String>;

/// A command from a map command.
#[derive(Debug)]
enum ShortcutCommand {
    /// A complete command that is to be executed.
    Complete(String),
    /// An incomplete command where the user needs to complete it and press Enter.
    Incomplete(String),
}

const TRANSPARENT: &'static GdkRGBA = &GdkRGBA { red: 0.0, green: 0.0, blue: 0.0, alpha: 0.0 };

const BLOCKING_INPUT_MODE: &'static str = "blocking-input";
const COMMAND_MODE: &'static str = "command";
const COMPLETE_NEXT_COMMAND: &'static str = "complete-next";
const COMPLETE_PREVIOUS_COMMAND: &'static str = "complete-previous";
const INPUT_MODE: &'static str = "input";
const NORMAL_MODE: &'static str = "normal";

/// Alias for an application builder without settings.
pub type SimpleApplicationBuilder = ApplicationBuilder<NoSettings>;

/// Application builder.
pub struct ApplicationBuilder<Sett: mg_settings::settings::Settings> {
    completers: HashMap<String, Box<Completer>>,
    modes: Option<Modes>,
    include_path: Option<PathBuf>,
    settings: Option<Sett>,
}

impl<Sett: mg_settings::settings::Settings + 'static> ApplicationBuilder<Sett> {
    /// Create a new application builder.
    #[allow(new_without_default_derive)]
    pub fn new() -> Self {
        ApplicationBuilder {
            completers: HashMap::new(),
            modes: None,
            include_path: None,
            settings: None,
        }
    }

    /// Create a new application with configuration and include path.
    pub fn build<Spec, Comm>(self) -> Box<Application<Comm, Sett, Spec>>
        where Spec: SpecialCommand + 'static,
              Comm: EnumFromStr + EnumMetaData + 'static,
              Sett: EnumMetaData + SettingCompletion,
    {
        Application::new(self)
    }

    /// Add a input completer.
    pub fn completer<C: Completer + 'static>(mut self, name: &str, completer: C) -> Self {
        self.completers.insert(name.to_string(), Box::new(completer));
        self
    }

    /// Set the include path of the configuration files.
    pub fn include_path<P: AsRef<Path>>(mut self, include_path: P) -> Self {
        self.include_path = Some(include_path.as_ref().to_path_buf());
        self
    }

    /// Set the configuration of the application.
    pub fn modes(mut self, mut modes: Modes) -> Self {
        assert!(modes.insert("n".to_string(), NORMAL_MODE.to_string()).is_none(), "Duplicate mode prefix n.");
        assert!(modes.insert("c".to_string(), COMMAND_MODE.to_string()).is_none(), "Duplicate mode prefix c.");
        self.modes = Some(modes);
        self
    }

    /// Set the default settings of the application.
    pub fn settings(mut self, settings: Sett) -> Self {
        self.settings = Some(settings);
        self
    }
}

/// Create a new MG application window.
/// This window contains a status bar where the user can type a command and a central widget.
pub struct Application<Comm, Sett: mg_settings::settings::Settings = NoSettings, Spec = NoSpecialCommands> {
    answer: Option<String>,
    command_callback: Option<Box<Fn(Comm)>>,
    choices: Vec<char>,
    close_callback: Option<Box<Fn()>>,
    current_command_mode: char,
    current_mode: String,
    current_shortcut: Vec<Key>,
    foreground_color: RGBA,
    input_callback: Option<Box<Fn(Option<String>)>>,
    mappings: HashMap<String, HashMap<Vec<Key>, String>>,
    modes: Modes,
    message: StatusBarItem,
    mode_changed_callback: Option<Box<Fn(&str)>>,
    mode_label: StatusBarItem,
    settings: Option<Sett>,
    settings_parser: Parser<Comm>,
    setting_change_callback: Option<Box<Fn(Sett::Variant)>>,
    shortcuts: HashMap<Key, String>,
    shortcut_label: StatusBarItem,
    shortcut_pressed: bool,
    special_command_callback: Option<Box<Fn(Spec)>>,
    status_bar: Box<StatusBar>,
    view: Overlay,
    variables: HashMap<String, Box<Fn() -> String>>,
    window: Window,
}

impl<Spec, Comm, Sett> Application<Comm, Sett, Spec>
    where Spec: SpecialCommand + 'static,
          Comm: EnumFromStr + EnumMetaData + 'static,
          Sett: mg_settings::settings::Settings + EnumMetaData + SettingCompletion + 'static,
{
    fn new(builder: ApplicationBuilder<Sett>) -> Box<Self> {
        let modes = builder.modes.unwrap_or_default();
        let config = Config {
            application_commands: vec![COMPLETE_NEXT_COMMAND.to_string(), COMPLETE_PREVIOUS_COMMAND.to_string()],
            mapping_modes: modes.keys().cloned().collect(),
        };

        let current_mode = NORMAL_MODE.to_string();

        let window = Window::new(WindowType::Toplevel);

        let grid = Grid::new();
        window.add(&grid);

        let view = Overlay::new();
        grid.attach(&view, 0, 0, 1, 1);

        let completion_view = CompletionView::new();
        view.add_overlay(&**completion_view);

        let mut completers: HashMap<String, Box<Completer>> = HashMap::new();
        completers.insert(DEFAULT_COMPLETER_IDENT.to_string(), Box::new(CommandCompleter::<Comm>::new()));
        completers.insert("set".to_string(), Box::new(SettingCompleter::<Sett>::new()));
        for (identifier, completer) in builder.completers {
            completers.insert(identifier, completer);
        }
        let mut status_bar = StatusBar::new(completion_view, completers);
        grid.attach(&**status_bar, 0, 2, 1, 1);
        window.show_all();
        status_bar.hide_widgets();
        status_bar.hide_completion();

        let foreground_color = Application::<Comm, Sett, Spec>::get_foreground_color(&window);

        let mut parser = Parser::new_with_config(config);
        if let Some(include_path) = builder.include_path {
            parser.set_include_path(include_path);
        }

        let mut app = Box::new(Application {
            answer: None,
            command_callback: None,
            choices: vec![],
            close_callback: None,
            current_command_mode: ':',
            current_mode: current_mode,
            current_shortcut: vec![],
            foreground_color: foreground_color,
            input_callback: None,
            mappings: HashMap::new(),
            modes: modes,
            message: StatusBarItem::new().left(),
            mode_changed_callback: None,
            mode_label: StatusBarItem::new().left(),
            settings: builder.settings,
            settings_parser: parser,
            setting_change_callback: None,
            shortcuts: HashMap::new(),
            shortcut_label: StatusBarItem::new(),
            shortcut_pressed: false,
            special_command_callback: None,
            status_bar: status_bar,
            view: view,
            variables: HashMap::new(),
            window: window,
        });

        app.status_bar.add_item(&app.shortcut_label);
        app.status_bar.add_item(&app.mode_label);
        app.status_bar.add_item(&app.message);

        connect!(app.window, connect_delete_event(_, _), app, quit);
        connect!(app.status_bar, connect_activate(input), app, command_activate(input));
        connect!(app.window, connect_key_press_event(_, key), app, key_press(key));
        connect!(app.window, connect_key_release_event(_, key), app, key_release(key));
        connect!(app.status_bar.entry, connect_changed(_), app, update_completions);

        app
    }

    /// Convert an action String to a command String.
    fn action_to_command(&self, action: &str) -> ShortcutCommand {
        if let Some(':') = action.chars().next() {
            if let Some(index) = action.find("<Enter>") {
                Complete(action[1..index].to_string())
            }
            else {
                Incomplete(action[1..].to_string())
            }
        }
        else {
            Complete(action.to_string())
        }
    }

    /// Create a new status bar item.
    pub fn add_statusbar_item(&self) -> StatusBarItem {
        let item = StatusBarItem::new();
        self.status_bar.add_item(&item);
        item
    }

    /// Add a variable that can be used in mappings.
    /// The placeholder will be replaced by the value return by the function.
    pub fn add_variable<F: Fn() -> String + 'static>(&mut self, variable_name: &str, function: F) {
        self.variables.insert(variable_name.to_string(), Box::new(function));
    }

    /// Handle an application command.
    fn app_command(&self, command: &str) {
        match command {
            COMPLETE_NEXT_COMMAND => self.status_bar.complete_next(),
            COMPLETE_PREVIOUS_COMMAND => self.status_bar.complete_previous(),
            _ => unreachable!(),
        }
    }

    /// Call the callback with the command or show an error if the command cannot be parsed.
    fn call_command(&mut self, command: Result<Command<Comm>>) {
        match command {
            Ok(command) => {
                match command {
                    App(command) => self.app_command(&command),
                    Custom(command) => {
                        if let Some(ref callback) = self.command_callback {
                            callback(command);
                        }
                    },
                    Set(name, value) => {
                        match Sett::to_variant(&name, value) {
                            Ok(setting) => self.set_setting(setting),
                            Err(error) => {
                                let message = format!("Error setting value: {}", error);
                                self.error(&message);
                            },
                        }
                    },
                    _ => unimplemented!(),
                }
            },
            Err(error) => {
                if let Some(error) = error.downcast_ref::<Error>() {
                    let message =
                        match error.typ {
                            MissingArgument => "Argument required".to_string(),
                            NoCommand => return,
                            Parse => format!("Parse error: unexpected {}, expecting: {}", error.unexpected, error.expected),
                            UnknownCommand => format!("Not a command: {}", error.unexpected),
                        };
                    self.error(&message);
                }
            },
        }
    }

    /// Call the setting changed callback.
    fn call_setting_callback(&self, setting: Sett::Variant) {
        if let Some(ref callback) = self.setting_change_callback {
            callback(setting);
        }
    }

    /// Handle the command entry activate event.
    fn command_activate(&mut self, input: Option<String>) {
        if self.current_mode == INPUT_MODE || self.current_mode == BLOCKING_INPUT_MODE {
            let mut should_reset = false;
            if let Some(ref callback) = self.input_callback {
                self.answer = input.clone();
                callback(input);
                should_reset = true;
            }
            if should_reset {
                self.reset();
            }
            self.input_callback = None;
            self.choices.clear();
        }
        else {
            self.handle_command(input);
        }
        self.return_to_normal_mode();
    }

    /// Handle the key press event for the command mode.
    #[allow(non_upper_case_globals)]
    fn command_key_press(&mut self, key: &EventKey) -> Inhibit {
        match key.get_keyval() {
            Escape => {
                self.return_to_normal_mode();
                self.reset();
                self.clear_shortcut();
                Inhibit(false)
            },
            _ => self.handle_shortcut(key),
        }
    }

    /// Handle the key release event for the command mode.
    fn command_key_release(&mut self, _key: &EventKey) -> Inhibit {
        if self.current_command_mode != ':' && Spec::is_always(self.current_command_mode) {
            if let Some(command) = self.status_bar.get_command() {
                self.handle_special_command(Current, &command);
            }
        }
        Inhibit(false)
    }

    /// Connect the close event to the specified callback.
    pub fn connect_close<F: Fn() + 'static>(&mut self, callback: F) {
        self.close_callback = Some(Box::new(callback));
    }

    /// Add a callback to the command event.
    pub fn connect_command<F: Fn(Comm) + 'static>(&mut self, callback: F) {
        self.command_callback = Some(Box::new(callback));
    }

    /// Add a callback to the window key press event.
    pub fn connect_key_press_event<F: Fn(&Window, &EventKey) -> Inhibit + 'static>(&self, callback: F) {
        self.window.connect_key_press_event(callback);
    }

    /// Add a callback to change mode event.
    pub fn connect_mode_changed<F: Fn(&str) + 'static>(&mut self, callback: F) {
        self.mode_changed_callback = Some(Box::new(callback));
    }

    /// Add a callback to setting changed event.
    pub fn connect_setting_changed<F: Fn(Sett::Variant) + 'static>(&mut self, callback: F) {
        self.setting_change_callback = Some(Box::new(callback));
    }

    /// Add a callback to the special command event.
    pub fn connect_special_command<F: Fn(Spec) + 'static>(&mut self, callback: F) {
        self.special_command_callback = Some(Box::new(callback));
    }

    /// Get the color of the text.
    fn get_foreground_color(window: &Window) -> RGBA {
        let style_context = window.get_style_context().unwrap();
        style_context.get_color(STATE_FLAG_NORMAL)
    }

    /// Get the current mode.
    pub fn get_mode(&self) -> &str {
        &self.current_mode
    }

    /// Handle the command activate event.
    fn handle_command(&mut self, command: Option<String>) {
        if let Some(command) = command {
            if self.current_command_mode == ':' {
                let result = self.settings_parser.parse_line(&command);
                self.call_command(result);
            }
            else {
                self.handle_special_command(Final, &command);
            }
        }
    }

    /// Handle a special command activate or key press event.
    fn handle_special_command(&mut self, activation_type: ActivationType, command: &str) {
        let mut update_identifier = false;
        if let Some(ref callback) = self.special_command_callback {
            if let Ok(special_command) = Spec::identifier_to_command(self.current_command_mode, command) {
                callback(special_command);
                if activation_type == Final {
                    update_identifier = true;
                }
            }
        }
        if update_identifier {
            self.set_current_identifier(':');
        }
    }

    /// Input the specified command.
    fn input_command(&mut self, command: &str) {
        self.set_mode(COMMAND_MODE);
        self.status_bar.show_entry();
        let mut command = command.to_string();
        for (variable, function) in &self.variables {
            command = command.replace(&format!("<{}>", variable), &function());
        }
        let text: Cow<str> =
            if command.contains(' ') {
                command.into()
            }
            else {
                format!("{} ", command).into()
            };
        self.status_bar.set_input(&text);
    }

    /// Handle the key press event for the input mode.
    #[allow(non_upper_case_globals)]
    fn input_key_press(&mut self, key: &EventKey) -> Inhibit {
        match key.get_keyval() {
            Escape => {
                self.return_to_normal_mode();
                self.reset();
                self.clear_shortcut();
                if let Some(ref callback) = self.input_callback {
                    callback(None);
                }
                self.input_callback = None;
                Inhibit(false)
            },
            keyval => {
                if self.handle_input_shortcut(key) {
                    return Inhibit(true);
                }
                else if let Some(character) = char::from_u32(keyval) {
                    if self.choices.contains(&character) {
                        self.set_dialog_answer(&character.to_string());
                        return Inhibit(true);
                    }
                }
                self.handle_shortcut(key)
            },
        }
    }

    /// Handle the key press event.
    fn key_press(&mut self, key: &EventKey) -> Inhibit {
        match self.current_mode.as_ref() {
            NORMAL_MODE => self.normal_key_press(key),
            COMMAND_MODE => self.command_key_press(key),
            BLOCKING_INPUT_MODE | INPUT_MODE => self.input_key_press(key),
            _ => self.handle_shortcut(key)
        }
    }

    /// Handle the key release event.
    fn key_release(&mut self, key: &EventKey) -> Inhibit {
        match self.current_mode.as_ref() {
            COMMAND_MODE => self.command_key_release(key),
            _ => Inhibit(false),
        }
    }

    /// Handle the key press event for the normal mode.
    #[allow(non_upper_case_globals)]
    fn normal_key_press(&mut self, key: &EventKey) -> Inhibit {
        match key.get_keyval() {
            colon => {
                self.status_bar.set_completer(DEFAULT_COMPLETER_IDENT);
                self.set_current_identifier(':');
                self.set_mode(COMMAND_MODE);
                self.reset();
                self.clear_shortcut();
                self.status_bar.show_completion();
                self.status_bar.show_entry();
                Inhibit(true)
            },
            Escape => {
                self.reset();
                self.clear_shortcut();
                self.handle_shortcut(key)
            },
            keyval => {
                let character = keyval as u8 as char;
                if Spec::is_identifier(character) {
                    self.status_bar.set_completer(NO_COMPLETER_IDENT);
                    self.set_current_identifier(character);
                    self.set_mode(COMMAND_MODE);
                    self.reset();
                    self.clear_shortcut();
                    self.status_bar.show_entry();
                    Inhibit(true)
                }
                else {
                    self.handle_shortcut(key)
                }
            },
        }
    }

    /// Parse a configuration file.
    pub fn parse_config<P: AsRef<Path>>(&mut self, filename: P) -> Result<()> {
        let file = File::open(filename)?;
        let buf_reader = BufReader::new(file);
        let commands = self.settings_parser.parse(buf_reader)?;
        for command in commands {
            match command {
                App(command) => self.app_command(&command),
                Custom(command) => {
                    if let Some(ref callback) = self.command_callback {
                        callback(command);
                    }
                },
                Map { action, keys, mode } => {
                    let mappings = self.mappings.entry(self.modes[&mode].clone()).or_insert_with(HashMap::new);
                    mappings.insert(keys, action);
                },
                Set(name, value) => self.set_setting(Sett::to_variant(&name, value)?),
                Unmap { .. } => panic!("not yet implemented"), // TODO
            }
        }
        Ok(())
    }

    /// Call the callback added by the user.
    /// Otherwise, exit the main loop.
    fn quit(&self) -> Inhibit {
        if let Some(ref callback) = self.close_callback {
            callback();
        }
        else {
            gtk::main_quit();
        }
        Inhibit(true)
    }

    /// Handle the escape event.
    fn reset(&mut self) {
        self.reset_colors();
        self.status_bar.hide_widgets();
        self.message.set_text("");
        self.show_mode();
        self.clear_shortcut();
    }

    /// Reset the background and foreground colors of the status bar.
    fn reset_colors(&self) {
        self.status_bar.override_background_color(STATE_FLAG_NORMAL, TRANSPARENT);
        self.status_bar.override_color(STATE_FLAG_NORMAL, &self.foreground_color);
    }

    /// Go back to the normal mode from command or input mode.
    fn return_to_normal_mode(&mut self) {
        self.status_bar.hide_entry();
        self.status_bar.hide_completion();
        self.set_mode(NORMAL_MODE);
        self.set_current_identifier(':');
    }

    /// Get the settings.
    pub fn settings(&self) -> &Sett {
        self.settings.as_ref().unwrap()
    }

    /// Set a setting value.
    pub fn set_setting(&mut self, setting: Sett::Variant) {
        if let Some(ref mut settings) = self.settings {
            settings.set_value(setting.clone());
        }
        self.call_setting_callback(setting);
    }

    /// Set the current (special) command identifier.
    fn set_current_identifier(&mut self, identifier: char) {
        self.current_command_mode = identifier;
        self.status_bar.set_identifier(&identifier.to_string());
    }

    /// Set the answer to return to the caller of the dialog.
    fn set_dialog_answer(&mut self, answer: &str) {
        let mut should_reset = false;
        if self.current_mode == BLOCKING_INPUT_MODE {
            self.answer = Some(answer.to_string());
            gtk::main_quit();
        }
        else if let Some(ref callback) = self.input_callback {
            callback(Some(answer.to_string()));
            self.choices.clear();
            should_reset = true;
        }
        if should_reset {
            self.return_to_normal_mode();
            self.reset();
        }
        self.input_callback = None;
    }

    /// Set the current mode.
    pub fn set_mode(&mut self, mode: &str) {
        self.current_mode = mode.to_string();
        self.show_mode();
        if let Some(ref callback) = self.mode_changed_callback {
            callback(mode);
        }
    }

    /// Set the main widget.
    pub fn set_view<W: IsA<Widget> + WidgetExt>(&self, view: &W) {
        view.set_hexpand(true);
        view.set_vexpand(true);
        view.show_all();
        self.view.add(view);
    }

    /// Set the window title.
    pub fn set_window_title(&self, title: &str) {
        self.window.set_title(title);
    }

    /// Show the current mode if it is not the normal mode.
    fn show_mode(&self) {
        if self.current_mode != NORMAL_MODE && self.current_mode != COMMAND_MODE && self.current_mode != INPUT_MODE && self.current_mode != BLOCKING_INPUT_MODE {
            self.mode_label.set_text(&self.current_mode);
        }
        else {
            self.mode_label.set_text("");
        }
    }

    /// Update the completions of the status bar.
    fn update_completions(&mut self) {
        self.status_bar.update_completions(&self.current_mode);
    }

    /// Use the dark variant of the theme if available.
    pub fn use_dark_theme(&mut self) {
        let settings = Settings::get_default().unwrap();
        settings.set_data("gtk-application-prefer-dark-theme", 1);
        self.foreground_color = Application::<Comm, Sett, Spec>::get_foreground_color(&self.window);
    }

    /// Get the application window.
    pub fn window(&self) -> &Window {
        &self.window
    }
}