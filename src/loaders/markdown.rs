use crate::Ingest;
#[cfg(feature = "generator")]
use crate::generator::Generator;
use bevy_app::{App, Plugin, Update};
use bevy_ecs::component::Component;
use bevy_ecs::entity::Entity;
use bevy_ecs::prelude::{Commands, Query, Resource};
use bevy_ecs::schedule::{IntoScheduleConfigs, SystemSet};
use bevy_ecs::system::Res;
use gray_matter::engine::YAML;
use gray_matter::{Matter, ParsedEntity};
use pulldown_cmark::{CowStr, Event, Options, Parser, Tag, html};
use serde::de::DeserializeOwned;
use std::error::Error;
use std::fs;
use std::fs::read_to_string;
use std::marker::PhantomData;
use std::path::Path;

/// System sets related to markdown and markdown to HTML conversion
#[derive(SystemSet, Clone, Debug, PartialEq, Eq, Hash)]
pub enum MarkdownSystems {
    /// System sets for systems reading markdown files
    Read,
    /// System set for systems converting markdown to HTML
    Convert,
}

struct ReadMarkdown<M: Ingest + DeserializeOwned + Sync + Send + 'static> {
    _marker: PhantomData<M>,
}

impl<M: Ingest + DeserializeOwned + Sync + Send + 'static> Plugin for ReadMarkdown<M> {
    fn build(&self, app: &mut App) {
        app.add_systems(
            Update,
            read_markdown_from_directories::<M>.in_set(MarkdownSystems::Read),
        );
    }
}

impl<M: Ingest + DeserializeOwned + Sync + Send + 'static> ReadMarkdown<M> {
    pub fn new() -> Self {
        Self {
            _marker: PhantomData,
        }
    }
}

/// Extension trait for the data layer containing markdown specific methods
pub trait MarkdownDataLayer {
    /// Add a directory to be loaded as a collection of markdown files
    fn add_markdown_directory<M: Ingest + DeserializeOwned + Sync + Send + 'static>(
        &mut self,
        directory: impl Into<String>,
    ) -> &mut Self;
}

#[cfg(feature = "generator")]
impl MarkdownDataLayer for Generator {
    fn add_markdown_directory<M: Ingest + DeserializeOwned + Sync + Send + 'static>(
        &mut self,
        directory: impl Into<String>,
    ) -> &mut Self {
        self.app.init_resource::<MarkdownDirectories<M>>();
        if !self.app.is_plugin_added::<ReadMarkdown<M>>() {
            self.add_plugins(ReadMarkdown::<M>::new());
        }

        let mut directories = self
            .app
            .world_mut()
            .resource_mut::<MarkdownDirectories<M>>();
        directories.directories.push(directory.into());

        self
    }
}

#[derive(Resource)]
struct MarkdownDirectories<M: Ingest + DeserializeOwned + Sync + Send + 'static> {
    directories: Vec<String>,
    _marker: PhantomData<M>,
}

impl<M: Ingest + DeserializeOwned + Sync + Send + 'static> Default for MarkdownDirectories<M> {
    fn default() -> Self {
        Self {
            directories: vec![],
            _marker: PhantomData,
        }
    }
}

fn read_markdown_from_directories<
    FrontMatter: Ingest + DeserializeOwned + Sync + Send + 'static,
>(
    mut commands: Commands,
    directories: Res<MarkdownDirectories<FrontMatter>>,
) {
    fn read_from_dir<FrontMatter: Ingest + DeserializeOwned + Sync + Send + 'static>(
        path: &Path,
        commands: &mut Commands,
    ) -> Result<Vec<Entity>, Box<dyn Error>> {
        let mut files = vec![];

        for entry in fs::read_dir(path)? {
            let entry = entry?;
            let path = entry.path();
            if path.is_file() {
                files.push(read_markdown::<FrontMatter>(&path, commands)?);
            } else if path.is_dir() {
                files.append(&mut read_from_dir::<FrontMatter>(&path, commands)?)
            }
        }
        Ok(files)
    }
    for directory in &directories.directories {
        let path = Path::new(directory);
        read_from_dir::<FrontMatter>(path, &mut commands)
            .unwrap_or_else(|e| panic!("Failed to read files from {}: {:?}", directory, e));
    }
}

fn read_markdown<FrontMatter: Ingest + DeserializeOwned + Sync + Send + 'static>(
    path: &Path,
    commands: &mut Commands,
) -> Result<Entity, Box<dyn Error>> {
    let file = read_to_string(path)?;
    let matter = Matter::<YAML>::new();
    let markdown: ParsedEntity = matter.parse(&file)?;
    let mut file = commands.spawn(());
    if let Some(front_matter) = markdown.data {
        let parsed_front_matter = front_matter.deserialize::<FrontMatter>()?;
        parsed_front_matter.ingest_path(&mut file, path);
        parsed_front_matter.ingest(&mut file);
    }
    file.insert(MarkdownBody(markdown.content));
    Ok(file.id())
}

/// Component containing Markdown
#[derive(Component, Clone)]
pub struct MarkdownBody(pub String);

/// Rewrite paths in markdown to reference copied assets in dist/
pub trait MarkdownAttachments: Default + Send + Sync + 'static {
    /// Base path of original attachments
    const BASE_PATH: Option<&'static str> = None;
}

/// Plugin to convert all markdown content to HTML
///
/// The plugin will query for all [`MarkdownBody`] components and add a [`Html`] component to
/// each of the entities.
#[derive(Default)]
pub struct ConvertMarkdownToHtml<A: MarkdownAttachments> {
    _p: PhantomData<A>,
}

impl<A: MarkdownAttachments> Plugin for ConvertMarkdownToHtml<A> {
    fn build(&self, app: &mut App) {
        app.configure_sets(
            Update,
            (MarkdownSystems::Read, MarkdownSystems::Convert).chain(),
        )
        .add_systems(
            Update,
            convert_markdown_to_html::<A>.in_set(MarkdownSystems::Convert),
        );
    }
}

fn convert_markdown_to_html<A: MarkdownAttachments>(
    markdown: Query<(Entity, &MarkdownBody)>,
    mut commands: Commands,
) {
    for (file, MarkdownBody(markdown)) in &markdown {
        let parser = Parser::new_ext(markdown, Options::all()).map(|ev| {
            if let Some(base_path) = A::BASE_PATH {
                match ev {
                    Event::Start(Tag::Image {
                        link_type,
                        dest_url,
                        title,
                        id,
                    }) => Event::Start(Tag::Image {
                        link_type,
                        dest_url: CowStr::from(rewrite_image(&dest_url, base_path)),
                        title,
                        id,
                    }),
                    other => other,
                }
            } else {
                ev
            }
        });
        let mut html = String::new();
        html::push_html(&mut html, parser);
        commands.entity(file).insert(Html(html));
    }
}

fn rewrite_image(dest: &str, base_path: &str) -> String {
    let is_local = !dest.contains("://") && !dest.starts_with('/') && !dest.starts_with("data:");
    if !is_local {
        return dest.to_string();
    }
    let file = dest.rsplit('/').next().unwrap_or(dest);
    format!("{}/{}", base_path.trim_end_matches('/'), file)
}

/// Component containing HTML
#[derive(Component, Clone)]
pub struct Html(pub String);
