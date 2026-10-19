//! Color themes. Each theme maps semantic UI roles to 256-color ANSI codes
//! (approximating truecolor) so triagent can look like Claude Code or any of
//! OpenCode's built-in themes.

const ESC: &str = "\x1b";

pub struct Theme {
    pub name: &'static str,
    pub bar_bg: &'static str, // "48;5;N" or ""
    pub bar_fg: &'static str, // "38;5;N" or ""
    pub user: &'static str,
    pub text: &'static str,
    pub dim: &'static str,
    pub tool: &'static str,
    pub err: &'static str,
    pub add: &'static str,
    pub del: &'static str,
    pub warn: &'static str,
    pub sel_bg: &'static str, // selection background "48;5;N"
    pub sel_fg: &'static str,
}

macro_rules! theme {
    ($name:expr, $bar_bg:expr, $bar_fg:expr, $user:expr, $text:expr, $dim:expr, $tool:expr, $err:expr, $add:expr, $del:expr, $warn:expr) => {
        Theme {
            name: $name,
            bar_bg: $bar_bg,
            bar_fg: $bar_fg,
            user: $user,
            text: $text,
            dim: $dim,
            tool: $tool,
            err: $err,
            add: $add,
            del: $del,
            warn: $warn,
            sel_bg: "48;5;240",
            sel_fg: "38;5;231",
        }
    };
}

pub static THEMES: &[Theme] = &[
    theme!(
        "claude", "48;5;236", "38;5;250", "38;5;51", "38;5;255", "38;5;245", "38;5;214",
        "38;5;196", "38;5;114", "38;5;196", "38;5;220"
    ),
    theme!(
        "opencode", "48;5;235", "38;5;250", "38;5;49", "38;5;253", "38;5;243", "38;5;180",
        "38;5;203", "38;5;78", "38;5;203", "38;5;221"
    ),
    theme!("system", "", "", "", "", "", "", "", "", "", ""),
    theme!(
        "tokyonight",
        "48;5;235",
        "38;5;189",
        "38;5;111",
        "38;5;189",
        "38;5;103",
        "38;5;180",
        "38;5;203",
        "38;5;114",
        "38;5;203",
        "38;5;221"
    ),
    theme!(
        "gruvbox", "48;5;235", "38;5;223", "38;5;142", "38;5;223", "38;5;245", "38;5;214",
        "38;5;167", "38;5;142", "38;5;167", "38;5;214"
    ),
    theme!(
        "nord", "48;5;236", "38;5;253", "38;5;110", "38;5;253", "38;5;247", "38;5;179", "38;5;131",
        "38;5;114", "38;5;131", "38;5;179"
    ),
    theme!(
        "catppuccin",
        "48;5;234",
        "38;5;189",
        "38;5;183",
        "38;5;189",
        "38;5;243",
        "38;5;221",
        "38;5;210",
        "38;5;114",
        "38;5;210",
        "38;5;221"
    ),
    theme!(
        "matrix", "48;5;16", "38;5;46", "38;5;46", "38;5;46", "38;5;40", "38;5;46", "38;5;196",
        "38;5;46", "38;5;196", "38;5;46"
    ),
    theme!(
        "one-dark", "48;5;235", "38;5;251", "38;5;75", "38;5;251", "38;5;244", "38;5;180",
        "38;5;167", "38;5;114", "38;5;167", "38;5;180"
    ),
    theme!(
        "everforest",
        "48;5;235",
        "38;5;223",
        "38;5;108",
        "38;5;223",
        "38;5;245",
        "38;5;179",
        "38;5;131",
        "38;5;108",
        "38;5;131",
        "38;5;179"
    ),
    theme!(
        "ayu", "48;5;233", "38;5;251", "38;5;215", "38;5;251", "38;5;245", "38;5;215", "38;5;167",
        "38;5;114", "38;5;167", "38;5;215"
    ),
];

impl Theme {
    pub fn by_name(name: &str) -> &'static Theme {
        THEMES.iter().find(|t| t.name == name).unwrap_or(&THEMES[0])
    }

    pub fn names() -> Vec<&'static str> {
        THEMES.iter().map(|t| t.name).collect()
    }

    fn s(&self, code: &str) -> String {
        if code.is_empty() {
            String::new()
        } else {
            format!("{ESC}[{code}m")
        }
    }

    pub fn bar_bg(&self) -> String {
        self.s(self.bar_bg)
    }
    pub fn bar_fg(&self) -> String {
        self.s(self.bar_fg)
    }
    pub fn user(&self) -> String {
        self.s(self.user)
    }
    pub fn text(&self) -> String {
        self.s(self.text)
    }
    pub fn dim(&self) -> String {
        self.s(self.dim)
    }
    pub fn tool(&self) -> String {
        self.s(self.tool)
    }
    pub fn err(&self) -> String {
        self.s(self.err)
    }
    pub fn add(&self) -> String {
        self.s(self.add)
    }
    pub fn del(&self) -> String {
        self.s(self.del)
    }
    pub fn warn(&self) -> String {
        self.s(self.warn)
    }
    pub fn selection(&self) -> (String, String) {
        (self.s(self.sel_bg), self.s(self.sel_fg))
    }
    pub fn reset(&self) -> String {
        format!("{ESC}[0m")
    }
}
