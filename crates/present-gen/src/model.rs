//! The window's Slint types, read with Slint's own parser: the exported enums and structs of
//! `ui/types.slint`, and the presenter's globals in `ui/state.slint` with their `in` properties.

use std::path::{Path, PathBuf};

use i_slint_compiler::diagnostics::BuildDiagnostics;
use i_slint_compiler::parser::{SyntaxKind, SyntaxNode, identifier_text, parse};
use i_slint_compiler::source_path::SourcePath;

/// The globals whose state the presenter owns, in no particular order: the others are the
/// window's own.
const PRESENTED: [&str; 10] = [
    "Session",
    "Composer",
    "Sessions",
    "Prompt",
    "Peek",
    "Side",
    "Nav",
    "Preferences",
    "Usage",
    "Transcript",
];

/// Properties of those globals that aren't state: the transcript's rows, which change one by one
/// (`TranscriptChange`), and the window's own counters, bumped through `Command`s.
const SKIPPED: [(&str, &str); 11] = [
    ("Transcript", "entries"),
    ("Transcript", "scroll-to-end-serial"),
    ("Transcript", "opened-serial"),
    ("Composer", "focus-serial"),
    ("Side", "focus-serial"),
    ("Sessions", "opened-serial"),
    ("Nav", "created-serial"),
    ("Nav", "show-window-controls"),
    ("Nav", "page-focus-serial"),
    ("Nav", "switcher-serial"),
    ("Nav", "new-project-serial"),
];

/// Why the model couldn't be read.
#[derive(Debug, thiserror::Error)]
pub enum ReadError {
    #[error("couldn't read {}", path.display())]
    Read {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("{} doesn't parse:\n{}", path.display(), diagnostics.join("\n"))]
    Syntax {
        path: PathBuf,
        diagnostics: Vec<String>,
    },
    #[error(
        "{owner}.{field} has the type `{written}`, which is neither a scalar nor one of types.slint's"
    )]
    UnknownType {
        owner: String,
        field: String,
        written: String,
    },
    #[error("{owner}.{field} has no type")]
    Untyped { owner: String, field: String },
}

/// A Slint scalar the model carries.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scalar {
    String,
    Int,
    Float,
    Length,
    Bool,
    StyledText,
    Image,
}

impl Scalar {
    fn named(written: &str) -> Option<Self> {
        match written {
            "string" => Some(Self::String),
            "int" => Some(Self::Int),
            "float" => Some(Self::Float),
            "length" => Some(Self::Length),
            "bool" => Some(Self::Bool),
            "styled-text" => Some(Self::StyledText),
            "image" => Some(Self::Image),
            _ => None,
        }
    }

    /// The Rust type the presentation model holds it as.
    pub fn rust(self) -> &'static str {
        match self {
            Self::String => "String",
            Self::Int => "i32",
            Self::Float | Self::Length => "f32",
            Self::Bool => "bool",
            Self::StyledText => "Styled",
            Self::Image => "Image",
        }
    }

    /// Whether it is a float, which has no `Eq`.
    pub fn is_float(self) -> bool {
        matches!(self, Self::Float | Self::Length)
    }
}

/// One value: a scalar, or an enum or struct of types.slint's, by name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Item {
    Scalar(Scalar),
    Named(String),
}

impl Item {
    fn rust(&self) -> &str {
        match self {
            Self::Scalar(scalar) => scalar.rust(),
            Self::Named(name) => name,
        }
    }
}

/// A field's or property's type: one value, or a list of them (`[T]`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Kind {
    One(Item),
    List(Item),
}

impl Kind {
    /// The Rust type the presentation model holds it as.
    pub fn rust(&self) -> String {
        match self {
            Self::One(item) => item.rust().to_owned(),
            Self::List(item) => format!("Vec<{}>", item.rust()),
        }
    }

    pub fn is_list(&self) -> bool {
        matches!(self, Self::List(_))
    }

    fn item(&self) -> &Item {
        match self {
            Self::One(item) | Self::List(item) => item,
        }
    }
}

/// A struct's field or a global's property, by its Slint name (`kebab-case`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Field {
    pub name: String,
    pub kind: Kind,
}

/// An enum, with its values in the order written; the first is the default.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Enum {
    pub name: String,
    pub values: Vec<String>,
}

/// A struct, or a presented global's state, with its fields in the order written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub name: String,
    pub fields: Vec<Field>,
}

/// Everything the generated files are written from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Model {
    pub enums: Vec<Enum>,
    pub structs: Vec<Record>,
    pub globals: Vec<Record>,
}

impl Model {
    /// Reads `types` (the enums and structs) and `state` (the globals) from the files.
    pub fn read(types: &Path, state: &Path) -> Result<Self, ReadError> {
        let types_source = read(types)?;
        let state_source = read(state)?;
        Self::parse(types, types_source, state, state_source)
    }

    /// The model from the two files' text; the paths only name them in errors.
    pub fn parse(
        types: &Path,
        types_source: String,
        state: &Path,
        state_source: String,
    ) -> Result<Self, ReadError> {
        let types_document = document(types, types_source)?;
        let state_document = document(state, state_source)?;

        let mut enums = Vec::new();
        let mut declared_structs = Vec::new();
        // Slint's syntax kinds are many and not ours, so they are compared, not matched; exported
        // components and the rest are skipped.
        for declaration in exported(&types_document) {
            if declaration.kind() == SyntaxKind::EnumDeclaration {
                enums.push(Enum {
                    name: declared_name(&declaration),
                    values: declaration
                        .children()
                        .filter(|child| child.kind() == SyntaxKind::EnumValue)
                        .filter_map(|value| identifier_text(&value))
                        .map(|value| value.to_string())
                        .collect(),
                });
            } else if declaration.kind() == SyntaxKind::StructDeclaration {
                declared_structs.push(declaration);
            }
        }
        let struct_names: Vec<String> = declared_structs.iter().map(declared_name).collect();
        let known = |written: &str| {
            enums.iter().any(|known| known.name == written)
                || struct_names.iter().any(|known| known == written)
        };

        let structs = declared_structs
            .iter()
            .map(|declaration| {
                let name = declared_name(declaration);
                let fields = declaration
                    .child_node(SyntaxKind::ObjectType)
                    .into_iter()
                    .flat_map(|object| object.children())
                    .filter(|member| member.kind() == SyntaxKind::ObjectTypeMember)
                    .map(|member| {
                        let field = identifier_text(&member).unwrap_or_default().to_string();
                        let kind = kind_of(&member, &name, &field, &known)?;
                        Ok(Field { name: field, kind })
                    })
                    .collect::<Result<_, ReadError>>()?;
                Ok(Record { name, fields })
            })
            .collect::<Result<_, ReadError>>()?;

        let globals = exported(&state_document)
            .filter(|declaration| {
                declaration.kind() == SyntaxKind::Component && is_global(declaration)
            })
            .map(|global| (declared_name(&global), global))
            .filter(|(name, _)| PRESENTED.contains(&name.as_str()))
            .map(|(name, global)| {
                let fields = global
                    .child_node(SyntaxKind::Element)
                    .into_iter()
                    .flat_map(|element| element.children())
                    .filter(|child| {
                        child.kind() == SyntaxKind::PropertyDeclaration && set_from_outside(child)
                    })
                    .map(|property| {
                        let field = property
                            .child_node(SyntaxKind::DeclaredIdentifier)
                            .and_then(|declared| identifier_text(&declared))
                            .unwrap_or_default()
                            .to_string();
                        (field, property)
                    })
                    .filter(|(field, _)| !SKIPPED.contains(&(name.as_str(), field.as_str())))
                    .map(|(field, property)| {
                        let kind = kind_of(&property, &name, &field, &known)?;
                        Ok(Field { name: field, kind })
                    })
                    .collect::<Result<_, ReadError>>()?;
                Ok(Record { name, fields })
            })
            .collect::<Result<_, ReadError>>()?;

        Ok(Self {
            enums,
            structs,
            globals,
        })
    }

    /// Whether a value of this type holds a float anywhere, through the structs it holds.
    pub fn has_float(&self, kind: &Kind) -> bool {
        self.item_has_float(kind.item(), &mut Vec::new())
    }

    fn item_has_float<'model>(&'model self, item: &Item, seen: &mut Vec<&'model str>) -> bool {
        match item {
            Item::Scalar(scalar) => scalar.is_float(),
            Item::Named(name) => match self.structs.iter().find(|record| &record.name == name) {
                Some(record) if !seen.contains(&record.name.as_str()) => {
                    seen.push(&record.name);
                    record
                        .fields
                        .iter()
                        .any(|field| self.item_has_float(field.kind.item(), seen))
                }
                Some(_) | None => false,
            },
        }
    }
}

fn read(path: &Path) -> Result<String, ReadError> {
    std::fs::read_to_string(path).map_err(|source| ReadError::Read {
        path: path.to_owned(),
        source,
    })
}

fn document(path: &Path, source: String) -> Result<SyntaxNode, ReadError> {
    let mut diagnostics = BuildDiagnostics::default();
    let document = parse(
        source,
        Some(SourcePath::File(path.to_owned())),
        None,
        &mut diagnostics,
    );
    if diagnostics.has_errors() {
        return Err(ReadError::Syntax {
            path: path.to_owned(),
            diagnostics: diagnostics.to_string_vec(),
        });
    }
    Ok(document)
}

/// What a document exports, in the order written.
fn exported(document: &SyntaxNode) -> impl Iterator<Item = SyntaxNode> {
    document
        .children()
        .filter(|child| child.kind() == SyntaxKind::ExportsList)
        .flat_map(|exports| exports.children())
}

fn declared_name(declaration: &SyntaxNode) -> String {
    declaration
        .child_node(SyntaxKind::DeclaredIdentifier)
        .and_then(|declared| identifier_text(&declared))
        .unwrap_or_default()
        .to_string()
}

/// The keywords written directly in a node, before its parts: `global`, or `in property`.
fn keywords(node: &SyntaxNode) -> impl Iterator<Item = String> {
    node.children_with_tokens()
        .filter_map(|child| child.as_token().cloned())
        .filter(|token| token.kind() == SyntaxKind::Identifier)
        .map(|token| token.text().to_owned())
}

fn is_global(component: &SyntaxNode) -> bool {
    keywords(component).any(|keyword| keyword == "global")
}

/// Whether a property is declared `in`: set from outside and only read inside, unlike `in-out`.
fn set_from_outside(property: &SyntaxNode) -> bool {
    keywords(property).eq(["in", "property"])
}

/// The type of a struct's member or a property, from its `Type` node.
fn kind_of(
    node: &SyntaxNode,
    owner: &str,
    field: &str,
    known: &impl Fn(&str) -> bool,
) -> Result<Kind, ReadError> {
    let untyped = || ReadError::Untyped {
        owner: owner.to_owned(),
        field: field.to_owned(),
    };
    let written = node.child_node(SyntaxKind::Type).ok_or_else(untyped)?;
    let item = |written: &SyntaxNode| {
        let name = written
            .child_node(SyntaxKind::QualifiedName)
            .and_then(|name| identifier_text(&name))
            .ok_or_else(untyped)?;
        match Scalar::named(&name) {
            Some(scalar) => Ok(Item::Scalar(scalar)),
            None if known(&name) => Ok(Item::Named(name.to_string())),
            None => Err(ReadError::UnknownType {
                owner: owner.to_owned(),
                field: field.to_owned(),
                written: written.text().to_string(),
            }),
        }
    };
    match written
        .child_node(SyntaxKind::ArrayType)
        .and_then(|array| array.child_node(SyntaxKind::Type))
    {
        Some(element) => item(&element).map(Kind::List),
        None => item(&written).map(Kind::One),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const TYPES: &str = r"
// A comment that mentions export struct Fake { nope: string }
export enum Tone { quiet, loud-and-clear }
enum Hidden { a }
export struct Point { x: length, label: string }
export struct Row { id: string, tones: [Tone], at: Point, picture: image, text: styled-text }
";

    const STATE: &str = r#"
import { Row, Tone } from "types.slint";
export global Session {
    in property <string> title: "x";
    in-out property <bool> chosen;
    out property <int> counted;
    in property <[Row]> rows;
    callback open(string);
}
export global Transcript {
    in property <bool> busy;
    in property <int> opened-serial;
}
export global Focus {
    in property <int> area;
}
"#;

    fn model() -> Model {
        Model::parse(
            Path::new("types.slint"),
            TYPES.to_owned(),
            Path::new("state.slint"),
            STATE.to_owned(),
        )
        .expect("the sample parses")
    }

    /// Exported enums keep their values in order, with Slint's names; comments and
    /// unexported declarations are ignored.
    #[test]
    fn reads_exported_enums_in_order() {
        assert_eq!(
            model().enums,
            [Enum {
                name: "Tone".into(),
                values: vec!["quiet".into(), "loud-and-clear".into()],
            }]
        );
    }

    /// Struct fields keep the order written, and each type is resolved: scalars, lists and
    /// types.slint's own names.
    #[test]
    fn reads_struct_fields_in_order_with_their_types() {
        let row = model()
            .structs
            .into_iter()
            .find(|record| record.name == "Row")
            .expect("Row");
        let kinds: Vec<_> = row
            .fields
            .iter()
            .map(|field| (field.name.as_str(), field.kind.rust()))
            .collect();
        assert_eq!(
            kinds,
            [
                ("id", "String".to_owned()),
                ("tones", "Vec<Tone>".to_owned()),
                ("at", "Point".to_owned()),
                ("picture", "Image".to_owned()),
                ("text", "Styled".to_owned()),
            ]
        );
    }

    /// Only presented globals are read, only their `in` properties, and not the skipped ones.
    #[test]
    fn reads_only_the_in_properties_of_presented_globals() {
        let globals = model().globals;
        let fields: Vec<_> = globals
            .iter()
            .map(|global| {
                (
                    global.name.as_str(),
                    global
                        .fields
                        .iter()
                        .map(|field| field.name.as_str())
                        .collect::<Vec<_>>(),
                )
            })
            .collect();
        assert_eq!(
            fields,
            [
                ("Session", vec!["title", "rows"]),
                ("Transcript", vec!["busy"])
            ]
        );
    }

    /// A float anywhere inside a type, through the structs it holds, is found.
    #[test]
    fn finds_floats_through_structs() {
        let model = model();
        assert!(model.has_float(&Kind::List(Item::Named("Row".into()))));
        assert!(!model.has_float(&Kind::One(Item::Named("Tone".into()))));
        assert!(!model.has_float(&Kind::One(Item::Scalar(Scalar::String))));
    }

    /// A type that is neither a scalar nor declared in types.slint is refused, naming where.
    #[test]
    fn refuses_unknown_types() {
        let error = Model::parse(
            Path::new("types.slint"),
            "export struct Bad { colour: brush }".to_owned(),
            Path::new("state.slint"),
            String::new(),
        )
        .expect_err("brush isn't carried");
        assert!(
            matches!(error, ReadError::UnknownType { owner, field, .. } if owner == "Bad" && field == "colour")
        );
    }

    /// A file that doesn't parse is refused with Slint's diagnostics.
    #[test]
    fn refuses_files_that_dont_parse() {
        let error = Model::parse(
            Path::new("types.slint"),
            "export struct {".to_owned(),
            Path::new("state.slint"),
            String::new(),
        )
        .expect_err("broken");
        assert!(matches!(error, ReadError::Syntax { diagnostics, .. } if !diagnostics.is_empty()));
    }
}
