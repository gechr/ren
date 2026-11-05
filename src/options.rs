//! Typed execution settings after syntax and precedence have been resolved.
use crate::{
    expressions::CompileOptions, rename::ExtensionScope, scan::FileSet,
    transforms::TransformOptions,
};
use std::path::PathBuf;

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    List,
    Preview,
    DryRun,
    Write,
}

pub(crate) struct Options {
    pub mode: Mode,
    pub paths: Vec<PathBuf>,
    pub null: bool,
    pub matching: CompileOptions,
    pub transforms: TransformOptions,
    pub scope: ExtensionScope,
    pub change_extension: Option<String>,
    pub file_set: Option<FileSet>,
    pub hidden: bool,
    pub no_ignore: bool,
    pub recursive: bool,
    pub include_dirs: bool,
    pub directory: Option<PathBuf>,
    pub parents: bool,
    pub create_dirs: bool,
}
