use ratatui::style::Color;

/// Built-in themes shown in Project Settings, in quick-toggle order.
///
/// The values are stable config-file spellings. Keep aliases in
/// [`Theme::normalize_name`] so older or hand-written configs remain usable.
pub(super) const BUILTIN_THEMES: &[(&str, &str)] = &[
    ("Dark", "dark"),
    ("Light", "light"),
    ("Nord", "nord"),
    ("Green", "green"),
    ("Solarized Light", "solarized"),
    ("Solarized Dark", "solarized-dark"),
    ("Dracula", "dracula"),
    ("Gruvbox Dark", "gruvbox-dark"),
    ("Catppuccin Mocha", "catppuccin-mocha"),
    ("Tokyo Night", "tokyo-night"),
    ("Rose Pine", "rose-pine"),
    ("Kanagawa", "kanagawa"),
    ("One Dark", "one-dark"),
    ("GitHub Dark", "github-dark"),
];

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub bg: Color,
    pub fg: Color,
    pub muted: Color,
    pub border: Color,
    /// Row background for the item the mouse sits on. Sits between `bg` and
    /// `border` so a preselected row reads as a hint next to the selection.
    pub hover: Color,
    pub focus: Color,
    pub warn: Color,
    pub ok: Color,
    pub err: Color,
    pub review: Color,
}

impl Theme {
    pub fn named(name: &str) -> Self {
        match Self::normalize_name(name) {
            "light" => Self::light(),
            "nord" => Self::nord(),
            "green" => Self::green(),
            "solarized" => Self::solarized(),
            "solarized-dark" => Self::solarized_dark(),
            "dracula" => Self::dracula(),
            "gruvbox-dark" => Self::gruvbox_dark(),
            "catppuccin-mocha" => Self::catppuccin_mocha(),
            "tokyo-night" => Self::tokyo_night(),
            "rose-pine" => Self::rose_pine(),
            "kanagawa" => Self::kanagawa(),
            "one-dark" => Self::one_dark(),
            "github-dark" => Self::github_dark(),
            _ => Self::dark(),
        }
    }

    pub fn normalize_name(name: &str) -> &'static str {
        match name {
            "light" | "textual-light" => "light",
            "nord" | "nordic" => "nord",
            "solarized" | "solarized-light" => "solarized",
            "solarized-dark" => "solarized-dark",
            "green" => "green",
            "dracula" => "dracula",
            "gruvbox-dark" => "gruvbox-dark",
            "catppuccin-mocha" => "catppuccin-mocha",
            "tokyo-night" => "tokyo-night",
            "rose-pine" => "rose-pine",
            "kanagawa" => "kanagawa",
            "one-dark" => "one-dark",
            "github-dark" => "github-dark",
            "dark" | "textual-dark" => "dark",
            _ => "dark",
        }
    }

    pub fn next_name(name: &str) -> &'static str {
        let current = Self::normalize_name(name);
        let index = BUILTIN_THEMES
            .iter()
            .position(|(_, value)| *value == current)
            .unwrap_or(0);
        BUILTIN_THEMES[(index + 1) % BUILTIN_THEMES.len()].1
    }

    fn dark() -> Self {
        Self {
            bg: Color::Rgb(18, 18, 22),
            fg: Color::Rgb(230, 230, 235),
            muted: Color::Rgb(130, 133, 145),
            border: Color::Rgb(72, 76, 92),
            hover: Color::Rgb(38, 40, 50),
            focus: Color::Rgb(106, 153, 255),
            warn: Color::Rgb(245, 190, 80),
            ok: Color::Rgb(100, 210, 140),
            err: Color::Rgb(240, 95, 120),
            review: Color::Rgb(210, 95, 180),
        }
    }

    fn light() -> Self {
        Self {
            bg: Color::Rgb(248, 248, 250),
            fg: Color::Rgb(28, 31, 38),
            muted: Color::Rgb(100, 105, 118),
            border: Color::Rgb(184, 190, 202),
            hover: Color::Rgb(226, 229, 236),
            focus: Color::Rgb(30, 100, 210),
            warn: Color::Rgb(180, 115, 0),
            ok: Color::Rgb(20, 135, 75),
            err: Color::Rgb(190, 45, 70),
            review: Color::Rgb(165, 45, 145),
        }
    }

    fn nord() -> Self {
        Self {
            bg: Color::Rgb(46, 52, 64),
            fg: Color::Rgb(236, 239, 244),
            muted: Color::Rgb(129, 143, 166),
            border: Color::Rgb(76, 86, 106),
            hover: Color::Rgb(59, 66, 82),
            focus: Color::Rgb(136, 192, 208),
            warn: Color::Rgb(235, 203, 139),
            ok: Color::Rgb(163, 190, 140),
            err: Color::Rgb(191, 97, 106),
            review: Color::Rgb(180, 142, 173),
        }
    }

    fn green() -> Self {
        Self {
            bg: Color::Rgb(7, 26, 13),
            fg: Color::Rgb(215, 255, 217),
            muted: Color::Rgb(112, 168, 120),
            border: Color::Rgb(45, 106, 59),
            hover: Color::Rgb(18, 53, 27),
            focus: Color::Rgb(101, 214, 138),
            warn: Color::Rgb(230, 200, 110),
            ok: Color::Rgb(103, 224, 138),
            err: Color::Rgb(240, 107, 107),
            review: Color::Rgb(192, 132, 252),
        }
    }

    fn solarized() -> Self {
        Self {
            bg: Color::Rgb(253, 246, 227),
            fg: Color::Rgb(101, 123, 131),
            muted: Color::Rgb(147, 161, 161),
            border: Color::Rgb(238, 232, 213),
            hover: Color::Rgb(245, 239, 218),
            focus: Color::Rgb(38, 139, 210),
            warn: Color::Rgb(181, 137, 0),
            ok: Color::Rgb(133, 153, 0),
            err: Color::Rgb(220, 50, 47),
            review: Color::Rgb(211, 54, 130),
        }
    }

    fn solarized_dark() -> Self {
        Self {
            bg: Color::Rgb(0, 43, 54),
            fg: Color::Rgb(131, 148, 150),
            muted: Color::Rgb(88, 110, 117),
            border: Color::Rgb(7, 54, 66),
            hover: Color::Rgb(8, 60, 72),
            focus: Color::Rgb(38, 139, 210),
            warn: Color::Rgb(181, 137, 0),
            ok: Color::Rgb(133, 153, 0),
            err: Color::Rgb(220, 50, 47),
            review: Color::Rgb(211, 54, 130),
        }
    }

    fn dracula() -> Self {
        Self {
            bg: Color::Rgb(40, 42, 54),
            fg: Color::Rgb(248, 248, 242),
            muted: Color::Rgb(98, 114, 164),
            border: Color::Rgb(68, 71, 90),
            hover: Color::Rgb(52, 55, 70),
            focus: Color::Rgb(189, 147, 249),
            warn: Color::Rgb(241, 250, 140),
            ok: Color::Rgb(80, 250, 123),
            err: Color::Rgb(255, 85, 85),
            review: Color::Rgb(255, 121, 198),
        }
    }

    fn gruvbox_dark() -> Self {
        Self {
            bg: Color::Rgb(40, 40, 40),
            fg: Color::Rgb(235, 219, 178),
            muted: Color::Rgb(168, 153, 132),
            border: Color::Rgb(80, 73, 69),
            hover: Color::Rgb(60, 56, 54),
            focus: Color::Rgb(131, 165, 152),
            warn: Color::Rgb(250, 189, 47),
            ok: Color::Rgb(184, 187, 38),
            err: Color::Rgb(251, 73, 52),
            review: Color::Rgb(211, 134, 155),
        }
    }

    fn catppuccin_mocha() -> Self {
        Self {
            bg: Color::Rgb(30, 30, 46),
            fg: Color::Rgb(205, 214, 244),
            muted: Color::Rgb(147, 153, 178),
            border: Color::Rgb(69, 71, 90),
            hover: Color::Rgb(49, 50, 68),
            focus: Color::Rgb(137, 180, 250),
            warn: Color::Rgb(249, 226, 175),
            ok: Color::Rgb(166, 227, 161),
            err: Color::Rgb(243, 139, 168),
            review: Color::Rgb(203, 166, 247),
        }
    }

    fn tokyo_night() -> Self {
        Self {
            bg: Color::Rgb(26, 27, 38),
            fg: Color::Rgb(192, 202, 245),
            muted: Color::Rgb(86, 95, 137),
            border: Color::Rgb(59, 66, 97),
            hover: Color::Rgb(36, 40, 59),
            focus: Color::Rgb(122, 162, 247),
            warn: Color::Rgb(224, 175, 104),
            ok: Color::Rgb(158, 206, 106),
            err: Color::Rgb(247, 118, 142),
            review: Color::Rgb(187, 154, 247),
        }
    }

    fn rose_pine() -> Self {
        Self {
            bg: Color::Rgb(25, 23, 36),
            fg: Color::Rgb(224, 222, 244),
            muted: Color::Rgb(144, 140, 170),
            border: Color::Rgb(64, 61, 82),
            hover: Color::Rgb(38, 35, 58),
            focus: Color::Rgb(196, 167, 231),
            warn: Color::Rgb(246, 193, 119),
            ok: Color::Rgb(156, 207, 216),
            err: Color::Rgb(235, 111, 146),
            review: Color::Rgb(235, 188, 186),
        }
    }

    fn kanagawa() -> Self {
        Self {
            bg: Color::Rgb(31, 31, 40),
            fg: Color::Rgb(220, 215, 186),
            muted: Color::Rgb(114, 113, 105),
            border: Color::Rgb(84, 84, 109),
            hover: Color::Rgb(42, 42, 55),
            focus: Color::Rgb(126, 156, 216),
            warn: Color::Rgb(230, 195, 132),
            ok: Color::Rgb(152, 187, 108),
            err: Color::Rgb(228, 104, 118),
            review: Color::Rgb(149, 127, 184),
        }
    }

    fn one_dark() -> Self {
        Self {
            bg: Color::Rgb(40, 44, 52),
            fg: Color::Rgb(171, 178, 191),
            muted: Color::Rgb(127, 132, 142),
            border: Color::Rgb(75, 82, 99),
            hover: Color::Rgb(44, 50, 60),
            focus: Color::Rgb(97, 175, 239),
            warn: Color::Rgb(229, 192, 123),
            ok: Color::Rgb(152, 195, 121),
            err: Color::Rgb(224, 108, 117),
            review: Color::Rgb(198, 120, 221),
        }
    }

    fn github_dark() -> Self {
        Self {
            bg: Color::Rgb(13, 17, 23),
            fg: Color::Rgb(230, 237, 243),
            muted: Color::Rgb(139, 148, 158),
            border: Color::Rgb(48, 54, 61),
            hover: Color::Rgb(22, 27, 34),
            focus: Color::Rgb(88, 166, 255),
            warn: Color::Rgb(210, 153, 34),
            ok: Color::Rgb(63, 185, 80),
            err: Color::Rgb(248, 81, 73),
            review: Color::Rgb(188, 140, 255),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_legacy_and_friendly_theme_names() {
        assert_eq!(Theme::normalize_name("textual-dark"), "dark");
        assert_eq!(Theme::normalize_name("textual-light"), "light");
        assert_eq!(Theme::normalize_name("nordic"), "nord");
        assert_eq!(Theme::normalize_name("solarized-light"), "solarized");
        assert_eq!(Theme::normalize_name("unknown"), "dark");
    }

    #[test]
    fn every_builtin_theme_has_a_distinct_background() {
        let backgrounds = BUILTIN_THEMES
            .iter()
            .map(|(_, name)| Theme::named(name).bg)
            .collect::<Vec<_>>();
        let unique = backgrounds.iter().collect::<std::collections::HashSet<_>>();
        assert_eq!(unique.len(), BUILTIN_THEMES.len());
    }

    #[test]
    fn quick_toggle_visits_all_builtin_themes() {
        let mut name = "dark";
        for (_, expected) in BUILTIN_THEMES
            .iter()
            .skip(1)
            .chain(std::iter::once(&BUILTIN_THEMES[0]))
        {
            name = Theme::next_name(name);
            assert_eq!(name, *expected);
        }
        assert_eq!(Theme::next_name("github-dark"), "dark");
    }
}
