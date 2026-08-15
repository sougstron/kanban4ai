use ratatui::style::Color;

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
        match name {
            "light" | "textual-light" => Self::light(),
            _ => Self::dark(),
        }
    }

    pub fn normalize_name(name: &str) -> &'static str {
        match name {
            "light" | "textual-light" => "light",
            _ => "dark",
        }
    }

    pub fn next_name(name: &str) -> &'static str {
        match Self::normalize_name(name) {
            "dark" => "light",
            _ => "dark",
        }
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
}
