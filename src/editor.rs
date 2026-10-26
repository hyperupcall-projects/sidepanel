//! Drafts and text input for the rectangle contents editor.
use crate::config::{Config, Rectangle};

pub const PAGE_SIZE: usize = 3;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field {
    SectionName,
    WebsiteTitle,
    WebsiteUrl,
}

#[derive(Debug, Clone, Default)]
pub struct TextInput {
    pub text: String,
    /// UTF-8 byte boundary.
    pub cursor: usize,
    pub selected_all: bool,
}

impl TextInput {
    pub fn new(text: String) -> Self {
        Self {
            cursor: text.len(),
            text,
            selected_all: false,
        }
    }

    pub fn insert(&mut self, text: &str) {
        if self.selected_all {
            self.text.clear();
            self.cursor = 0;
            self.selected_all = false;
        }
        let text: String = text.chars().filter(|c| !c.is_control()).collect();
        self.text.insert_str(self.cursor, &text);
        self.cursor += text.len();
    }

    pub fn move_cursor(&mut self, right: bool) {
        if self.selected_all {
            self.cursor = if right { self.text.len() } else { 0 };
            self.selected_all = false;
        } else if right {
            if let Some(c) = self.text[self.cursor..].chars().next() {
                self.cursor += c.len_utf8();
            }
        } else if let Some((index, _)) = self.text[..self.cursor].char_indices().next_back() {
            self.cursor = index;
        }
    }

    pub fn delete(&mut self, backwards: bool) {
        if self.selected_all {
            self.insert("");
        } else if backwards {
            let end = self.cursor;
            self.move_cursor(false);
            self.text.replace_range(self.cursor..end, "");
        } else if let Some(c) = self.text[self.cursor..].chars().next() {
            self.text.drain(self.cursor..self.cursor + c.len_utf8());
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Editor {
    pub rectangle: Rectangle,
    /// Global section index; website index is local to that section.
    pub section: Option<usize>,
    pub website: Option<usize>,
    pub adding_website: bool,
    pub section_page: usize,
    pub website_page: usize,
    pub focus: Option<Field>,
    pub name: TextInput,
    pub title: TextInput,
    pub url: TextInput,
    pub message: String,
}

impl Editor {
    pub fn sections(&self, config: &Config) -> Vec<usize> {
        config
            .sections
            .iter()
            .enumerate()
            .filter_map(|(i, s)| (s.rectangle == self.rectangle).then_some(i))
            .collect()
    }

    pub fn select_rectangle(&mut self, rectangle: Rectangle, config: &Config) {
        *self = Self {
            rectangle,
            ..Self::default()
        };
        if let Some(&index) = self.sections(config).first() {
            self.select_section(index, config);
        }
    }

    pub fn select_section(&mut self, index: usize, config: &Config) {
        let Some(section) = config
            .sections
            .get(index)
            .filter(|s| s.rectangle == self.rectangle)
        else {
            return;
        };
        self.section = Some(index);
        self.name = TextInput::new(section.title.clone());
        self.website_page = 0;
        self.clear_website();
        if !section.websites.is_empty() {
            self.select_website(0, config);
        }
    }

    pub fn clear_website(&mut self) {
        self.website = None;
        self.adding_website = false;
        self.title = TextInput::default();
        self.url = TextInput::default();
        self.focus = None;
        self.message.clear();
    }

    pub fn select_website(&mut self, index: usize, config: &Config) {
        let Some(site) = self
            .section
            .and_then(|s| config.sections.get(s))
            .and_then(|s| s.websites.get(index))
        else {
            return;
        };
        self.clear_website();
        self.website = Some(index);
        self.title = TextInput::new(site.title.clone().unwrap_or_default());
        self.url = TextInput::new(site.url.clone());
    }

    /// Apply an editor control. Return true only when saved content changed.
    pub fn apply(&mut self, action: crate::panel::Action, config: &mut Config) -> bool {
        use crate::config::{Section, Website};
        use crate::panel::Action;
        self.message.clear();
        match action {
            Action::SelectRectangle(rectangle) => self.select_rectangle(rectangle, config),
            Action::SelectSection(index) => self.select_section(index, config),
            Action::SelectWebsite(index) => self.select_website(index, config),
            Action::FocusField(field) => {
                self.focus = Some(field);
                let input = self.input_mut(field);
                input.cursor = input.text.len();
            }
            Action::AddSection => {
                let index = config.sections.len();
                config.sections.push(Section {
                    rectangle: self.rectangle,
                    title: "New section".into(),
                    websites: Vec::new(),
                });
                self.section_page = self.sections(config).len().saturating_sub(1) / PAGE_SIZE;
                self.select_section(index, config);
                self.focus = Some(Field::SectionName);
                self.name.selected_all = true;
                return true;
            }
            Action::RemoveSection => {
                let Some(index) = self.section.filter(|&i| i < config.sections.len()) else {
                    return false;
                };
                config.sections.remove(index);
                self.select_rectangle(self.rectangle, config);
                return true;
            }
            Action::SaveSection => {
                let Some(section) = self.section.and_then(|i| config.sections.get_mut(i)) else {
                    return false;
                };
                let name = self.name.text.trim();
                if name.is_empty() {
                    self.message = "Enter a section name.".into();
                    return false;
                }
                section.title = name.into();
                self.name = TextInput::new(name.into());
                return true;
            }
            Action::SectionPage(delta) => {
                let last = self.sections(config).len().saturating_sub(1) / PAGE_SIZE;
                self.section_page = self
                    .section_page
                    .saturating_add_signed(delta as isize)
                    .min(last);
            }
            Action::WebsitePage(delta) => {
                let last = self
                    .section
                    .and_then(|i| config.sections.get(i))
                    .map_or(0, |s| s.websites.len())
                    .saturating_sub(1)
                    / PAGE_SIZE;
                self.website_page = self
                    .website_page
                    .saturating_add_signed(delta as isize)
                    .min(last);
            }
            Action::AddWebsite => {
                if self.section.is_none() {
                    return false;
                }
                self.clear_website();
                self.adding_website = true;
                self.focus = Some(Field::WebsiteTitle);
            }
            Action::RemoveWebsite => {
                let Some(section) = self.section.and_then(|i| config.sections.get_mut(i)) else {
                    return false;
                };
                let Some(index) = self.website.filter(|&i| i < section.websites.len()) else {
                    return false;
                };
                section.websites.remove(index);
                self.website_page = self
                    .website_page
                    .min(section.websites.len().saturating_sub(1) / PAGE_SIZE);
                let next = index.min(section.websites.len().saturating_sub(1));
                self.clear_website();
                if !section.websites.is_empty() {
                    self.select_website(next, config);
                }
                return true;
            }
            Action::SaveWebsite => {
                let site = match Website::edited(&self.title.text, &self.url.text) {
                    Ok(site) => site,
                    Err(err) => {
                        self.message = err.to_string();
                        return false;
                    }
                };
                let Some(section) = self.section.and_then(|i| config.sections.get_mut(i)) else {
                    return false;
                };
                let index = if self.adding_website {
                    section.websites.push(site);
                    section.websites.len() - 1
                } else if let Some(index) = self.website.filter(|&i| i < section.websites.len()) {
                    section.websites[index] = site;
                    index
                } else {
                    return false;
                };
                self.website_page = index / PAGE_SIZE;
                self.select_website(index, config);
                return true;
            }
            _ => {}
        }
        false
    }

    pub fn input(&self, field: Field) -> &TextInput {
        match field {
            Field::SectionName => &self.name,
            Field::WebsiteTitle => &self.title,
            Field::WebsiteUrl => &self.url,
        }
    }

    pub fn input_mut(&mut self, field: Field) -> &mut TextInput {
        match field {
            Field::SectionName => &mut self.name,
            Field::WebsiteTitle => &mut self.title,
            Field::WebsiteUrl => &mut self.url,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn manage_sections_and_website_drafts_then_reload() {
        use crate::panel::Action;
        let mut config = Config::default();
        let original_url = config.sections[0].websites[0].url.clone();
        let mut editor = Editor::default();
        editor.select_rectangle(Rectangle::Right, &config);
        assert!(editor.apply(Action::AddSection, &mut config));
        editor.name = TextInput::new("Research".into());
        assert!(editor.apply(Action::SaveSection, &mut config));
        assert!(!editor.apply(Action::AddWebsite, &mut config));
        editor.title = TextInput::new("Rust docs".into());
        editor.url = TextInput::new("not a URL".into());
        assert!(!editor.apply(Action::SaveWebsite, &mut config));
        assert!(!editor.message.is_empty());
        assert!(config.sections[1].websites.is_empty());
        editor.url = TextInput::new("https://doc.rust-lang.org/".into());
        assert!(editor.apply(Action::SaveWebsite, &mut config));
        assert_eq!(editor.website, Some(0));
        assert!(!editor.adding_website);
        editor.title = TextInput::new("Rust".into());
        editor.url = TextInput::new("https://www.rust-lang.org/".into());
        assert!(editor.apply(Action::SaveWebsite, &mut config));
        let saved = Config::parse(&toml::to_string_pretty(&config).unwrap()).unwrap();
        assert_eq!(saved.sections[1].title, "Research");
        assert_eq!(saved.sections[1].rectangle, Rectangle::Right);
        assert_eq!(saved.sections[1].websites[0].title.as_deref(), Some("Rust"));
        assert_eq!(
            saved.sections[1].websites[0].url,
            "https://www.rust-lang.org/"
        );
        assert_eq!(saved.sections[0].websites[0].url, original_url);
        assert!(editor.apply(Action::RemoveWebsite, &mut config));
        assert!(config.sections[1].websites.is_empty());
        assert_eq!(editor.website, None);
        assert!(editor.apply(Action::RemoveSection, &mut config));
        assert_eq!(editor.section, None);
        assert_eq!(config.sections.len(), 1);
        editor.select_rectangle(Rectangle::Left, &config);
        assert!(editor.apply(Action::RemoveSection, &mut config));
        assert!(Config::parse(&toml::to_string(&config).unwrap())
            .unwrap()
            .sections
            .is_empty());
    }

    #[test]
    fn editing_unicode_and_selection_keeps_valid_boundaries() {
        let mut input = TextInput::new("Café 🦊".into());
        input.delete(true);
        assert_eq!(input.text, "Café ");
        input.move_cursor(false);
        input.move_cursor(false);
        input.delete(false);
        assert_eq!(input.text, "Caf ");
        input.selected_all = true;
        input.insert("https://example.com/\n");
        assert_eq!(input.text, "https://example.com/");
        assert_eq!(input.cursor, input.text.len());
    }

    #[test]
    fn switching_rectangles_and_sections_clears_stale_drafts() {
        let config = Config::default();
        let mut editor = Editor::default();
        editor.select_rectangle(Rectangle::Left, &config);
        assert_eq!(editor.section, Some(0));
        assert_eq!(editor.website, Some(0));
        editor.adding_website = true;
        editor.focus = Some(Field::WebsiteUrl);
        editor.select_rectangle(Rectangle::Right, &config);
        assert_eq!(editor.section, None);
        assert_eq!(editor.website, None);
        assert!(!editor.adding_website);
        assert_eq!(editor.url.text, "");
        assert_eq!(editor.focus, None);
    }
}
