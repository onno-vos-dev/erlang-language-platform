/*
 * Copyright (c) Meta Platforms, Inc. and affiliates.
 *
 * This source code is licensed under both the MIT license found in the
 * LICENSE-MIT file in the root directory of this source tree and the Apache
 * License, Version 2.0 found in the LICENSE-APACHE file in the root directory
 * of this source tree.
 */

use std::sync::Arc;

use elp_project_model::AppName;
use elp_syntax::ast::SourceFile;
use elp_syntax::Parse;
use elp_syntax::TextRange;
use elp_syntax::TextSize;

mod change;
mod input;
mod module_index;

// ---------------------------------------------------------------------
// Public API

pub mod fixture;
pub mod test_fixture;
pub mod test_utils;
pub use change::Change;
pub use elp_project_model::AppType;
pub use input::AppData;
pub use input::AppRoots;
pub use input::AppStructure;
pub use input::FileSource;
pub use input::IncludeOtp;
pub use input::ProjectApps;
pub use input::ProjectData;
pub use input::ProjectId;
pub use input::SourceRoot;
pub use input::SourceRootId;
pub use module_index::ModuleIndex;
pub use module_index::ModuleName;
pub use module_index::Modules;
pub use paths::AbsPath;
pub use paths::AbsPathBuf;
pub use paths::RelPath;
pub use paths::RelPathBuf;
pub use salsa;
pub use vfs::file_set::FileSet;
pub use vfs::file_set::FileSetConfig;
pub use vfs::file_set::FileSetConfigBuilder;
pub use vfs::loader;
pub use vfs::AnchoredPath;
pub use vfs::AnchoredPathBuf;
pub use vfs::ChangeKind;
pub use vfs::ChangedFile;
pub use vfs::FileId;
pub use vfs::Vfs;
pub use vfs::VfsPath;

// ---------------------------------------------------------------------

#[macro_export]
macro_rules! impl_intern_key {
    ($name:ident) => {
        impl $crate::salsa::InternKey for $name {
            fn from_intern_id(v: $crate::salsa::InternId) -> Self {
                $name(v)
            }
            fn as_intern_id(&self) -> $crate::salsa::InternId {
                self.0
            }
        }
    };
}

pub trait Upcast<T: ?Sized> {
    fn upcast(&self) -> &T;
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FilePosition {
    pub file_id: FileId,
    pub offset: TextSize,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Hash)]
pub struct FileRange {
    pub file_id: FileId,
    pub range: TextRange,
}

pub trait FileLoader {
    /// Text of the file.
    fn file_text(&self, file_id: FileId) -> Arc<String>;
}

/// Database which stores all significant input facts: source code and project
/// model. Everything else in ELP is derived from these queries.
#[salsa::query_group(SourceDatabaseStorage)]
pub trait SourceDatabase: FileLoader + salsa::Database {
    /// Path to a file, relative to the root of its source root.
    /// Source root of the file.
    #[salsa::input]
    fn file_source_root(&self, file_id: FileId) -> SourceRootId;

    /// Contents of the source root.
    #[salsa::input]
    fn source_root(&self, id: SourceRootId) -> Arc<SourceRoot>;

    #[salsa::input]
    fn app_data(&self, id: SourceRootId) -> Option<Arc<AppData>>;

    #[salsa::input]
    fn project_data(&self, id: ProjectId) -> Arc<ProjectData>;

    /// A global ID used to invalidate the database when making untracked changes.
    #[salsa::input]
    fn include_files_revision(&self) -> u64;

    /// Returns a map from module name to FileId of the containing file.
    fn module_index(&self, project_id: ProjectId) -> Arc<ModuleIndex>;

    /// Parse the file_id to AST
    fn parse(&self, file_id: FileId) -> Parse<SourceFile>;

    fn is_generated(&self, file_id: FileId) -> bool;

    fn is_test_suite_or_test_helper(&self, file_id: FileId) -> Option<bool>;

    fn file_app_type(&self, file_id: FileId) -> Option<AppType>;

    fn file_app_name(&self, file_id: FileId) -> Option<AppName>;
}

fn module_index(db: &dyn SourceDatabase, project_id: ProjectId) -> Arc<ModuleIndex> {
    let mut builder = ModuleIndex::builder();

    let project_data = db.project_data(project_id);
    for &source_root_id in &project_data.source_roots {
        if let Some(app_data) = db.app_data(source_root_id) {
            let source_root = db.source_root(source_root_id);
            for (file_id, file_source, path) in source_root.iter_app_files(&app_data) {
                if let Some((name, Some("erl"))) = path.name_and_extension() {
                    builder.insert(file_id, file_source, ModuleName::new(name));
                }
            }
        }
    }

    project_data
        .otp_project_id
        .iter()
        .for_each(|otp_project_id| {
            if *otp_project_id == project_id {
                builder.is_otp()
            } else {
                builder.set_otp(db.module_index(*otp_project_id))
            }
        });

    builder.build()
}

fn parse(db: &dyn SourceDatabase, file_id: FileId) -> Parse<SourceFile> {
    let text = db.file_text(file_id);
    SourceFile::parse_text(&text)
}

fn is_generated(db: &dyn SourceDatabase, file_id: FileId) -> bool {
    let contents = db.file_text(file_id);
    contents[0..(2001.min(contents.len()))].contains(&format!("{}generated", "@"))
}

fn is_test_suite_or_test_helper(db: &dyn SourceDatabase, file_id: FileId) -> Option<bool> {
    let root_id = db.file_source_root(file_id);
    let root = db.source_root(root_id);
    let app_data = db.app_data(root_id)?;
    let path = root.path_for_file(&file_id)?;
    if app_data.is_extra_src_file(path) {
        Some(true)
    } else {
        Some(false)
    }
}

fn file_app_type(db: &dyn SourceDatabase, file_id: FileId) -> Option<AppType> {
    let app_data = db.app_data(db.file_source_root(file_id))?;
    Some(app_data.app_type)
}

fn file_app_name(db: &dyn SourceDatabase, file_id: FileId) -> Option<AppName> {
    let app_data = db.app_data(db.file_source_root(file_id))?;
    Some(app_data.name.clone())
}

/// We don't want to give HIR knowledge of source roots, hence we extract these
/// methods into a separate DB.
#[salsa::query_group(SourceDatabaseExtStorage)]
pub trait SourceDatabaseExt: SourceDatabase {
    #[salsa::input]
    fn file_text(&self, file_id: FileId) -> Arc<String>;
}

/// Silly workaround for cyclic deps between the traits
pub struct FileLoaderDelegate<T>(pub T);

impl<T: SourceDatabaseExt> FileLoader for FileLoaderDelegate<&'_ T> {
    fn file_text(&self, file_id: FileId) -> Arc<String> {
        SourceDatabaseExt::file_text(self.0, file_id)
    }
}

/// If the `input` string represents an atom, and needs quoting, quote
/// it.
pub fn to_quoted_string(input: &str) -> String {
    fn is_valid_atom(input: &str) -> bool {
        let mut chars = input.chars();
        chars.next().map_or(false, |c| c.is_lowercase())
            && chars.all(|c| char::is_alphanumeric(c) || c == '_' || c == '@')
    }
    if is_valid_atom(input) {
        input.to_string()
    } else {
        format!("'{}'", &input)
    }
}
