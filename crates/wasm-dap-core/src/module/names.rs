use std::collections::HashMap;

use wasmparser::{Name, NameMap, NameSectionReader};

/// Contents of the `name` custom section.
#[derive(Debug, Default, Clone)]
pub struct Names {
    pub module: Option<String>,
    pub functions: HashMap<u32, String>,
    pub locals: HashMap<u32, HashMap<u32, String>>,
    pub globals: HashMap<u32, String>,
    pub memories: HashMap<u32, String>,
    pub tables: HashMap<u32, String>,
}

impl Names {
    pub(crate) fn read(&mut self, reader: NameSectionReader<'_>) {
        for subsection in reader.into_iter().flatten() {
            match subsection {
                Name::Module { name, .. } => self.module = Some(name.to_string()),
                Name::Function(map) => collect(map, &mut self.functions),
                Name::Global(map) => collect(map, &mut self.globals),
                Name::Memory(map) => collect(map, &mut self.memories),
                Name::Table(map) => collect(map, &mut self.tables),
                Name::Local(indirect) => {
                    for entry in indirect.into_iter().flatten() {
                        let target = self.locals.entry(entry.index).or_default();
                        collect(entry.names, target);
                    }
                }
                _ => {}
            }
        }
    }
}

fn collect(map: NameMap<'_>, target: &mut HashMap<u32, String>) {
    for naming in map.into_iter().flatten() {
        target.insert(naming.index, naming.name.to_string());
    }
}

/// Demangles Rust (legacy and v0) and Itanium C++ symbol names; other names are returned as-is.
pub fn demangle(name: &str) -> String {
    if let Ok(symbol) = rustc_demangle::try_demangle(name) {
        return format!("{symbol:#}");
    }
    if name.starts_with("_Z")
        && let Ok(symbol) = cpp_demangle::Symbol::new(name)
        && let Ok(text) = symbol.demangle()
    {
        return text;
    }
    name.to_string()
}
