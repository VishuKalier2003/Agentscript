use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::index::{Snapshot, SourceFile};
use super::outline::{Outline, Symbol};
use crate::scope::{folder_of, folder_prefix};

/** Build and manifest files that mark the root of a service or deployable unit */
const MANIFESTS: &[&str] = &[
    "pom.xml",
    "build.gradle",
    "build.gradle.kts",
    "package.json",
    "go.mod",
    "Cargo.toml",
    "pyproject.toml",
    "setup.py",
    "setup.cfg",
    "requirements.txt",
    "CMakeLists.txt",
    "Dockerfile",
];

/** Top-level folders whose direct children are services by convention */
const SERVICE_LAYOUTS: &[&str] = &["services", "apps", "cmd", "packages", "microservices"];

/** One symbol of the inventory with its relationships (indices into Graph::entities)
 * Fields
    - id: String - stable semantic id, "symbol:LANGUAGE:NAMESPACE.QUALIFIED" (see symbol_id)
    - location: String - "PATH#QUALIFIED#N", the key incremental relinking uses
    - file: usize - index into Snapshot::files
    - symbol: usize - index into that file's outline symbols
    - module: usize - index into Graph::modules
    - callees: Vec<usize> - entities it calls
    - callers: Vec<usize> - entities calling it
    - ambiguous: Vec<String> - called names matching several definitions, left unlinked
    - tested_by: Vec<String> - ids of test symbols, or "file:PATH" of test files, exercising it
    - test: bool - whether it is itself a test
    - contracts: Vec<String> - "POLICY:RULE" of every contract clause covering it
    - risk: Vec<String> - risk signals
    - score: u32 - weighted sum of the risk signals
*/
pub(crate) struct Entity {
    pub(crate) id: String,
    pub(crate) location: String,
    pub(crate) file: usize,
    pub(crate) symbol: usize,
    pub(crate) module: usize,
    pub(crate) callees: Vec<usize>,
    pub(crate) callers: Vec<usize>,
    pub(crate) ambiguous: Vec<String>,
    pub(crate) tested_by: Vec<String>,
    pub(crate) test: bool,
    pub(crate) contracts: Vec<String>,
    pub(crate) risk: Vec<String>,
    pub(crate) score: u32,
}

/** What the inventory knows about one file beyond its outline
 * Fields
    - module: usize - index into Graph::modules, None-like usize::MAX for unknown languages
    - service: usize - index into Graph::services
    - owners: Vec<String> - owners from CODEOWNERS
    - test: bool - whether it is a test file
    - tests: BTreeSet<usize> - for a test file, the files it exercises
    - tested_by: BTreeSet<usize> - test files exercising it
*/
pub(crate) struct FileInfo {
    pub(crate) module: usize,
    pub(crate) service: usize,
    pub(crate) owners: Vec<String>,
    pub(crate) test: bool,
    pub(crate) tests: BTreeSet<usize>,
    pub(crate) tested_by: BTreeSet<usize>,
}

/** A module or package: the unit a language groups code by (Java/Kotlin package, Go package
 * folder, Python or JavaScript module path, Rust module path, C++ namespace or folder)
 * Fields
    - id: String - "module:LANGUAGE:NAME"
    - language: &'static str - language
    - name: String - module name
    - files: usize - files in it
    - symbols: usize - symbols in it
    - service: usize - index into Graph::services of its first file
    - depends_on: BTreeMap<usize, usize> - modules it calls into, with call link counts
    - imports: BTreeSet<String> - everything its files import
*/
pub(crate) struct Module {
    pub(crate) id: String,
    pub(crate) language: &'static str,
    pub(crate) name: String,
    pub(crate) files: usize,
    pub(crate) symbols: usize,
    pub(crate) service: usize,
    pub(crate) depends_on: BTreeMap<usize, usize>,
    pub(crate) imports: BTreeSet<String>,
}

/** One folder and how its code relates to other folders
 * Fields
    - path: String - folder path, "" for the repository root
    - files: usize - files directly in it
    - files_total: usize - files in it and below
    - symbols_total: usize - symbols in it and below
    - children: BTreeSet<String> - direct subfolders
    - depends_on: BTreeMap<String, usize> - folders its files call into, with call link counts
*/
pub(crate) struct Folder {
    pub(crate) path: String,
    pub(crate) files: usize,
    pub(crate) files_total: usize,
    pub(crate) symbols_total: usize,
    pub(crate) children: BTreeSet<String>,
    pub(crate) depends_on: BTreeMap<String, usize>,
}

/** A service or deployable-unit candidate
 * Fields
    - id: String - "service:PATH" ("service:." for the repository root)
    - path: String - root folder, "" for the repository root
    - evidence: Vec<String> - manifests found, or the conventional layout that suggested it
    - files: usize - files belonging to it (by nearest service root)
    - symbols: usize - symbols in those files
    - languages: BTreeMap<&'static str, usize> - files per language
    - depends_on: BTreeMap<usize, usize> - services it calls into, with call link counts
*/
pub(crate) struct Service {
    pub(crate) id: String,
    pub(crate) path: String,
    pub(crate) evidence: Vec<String>,
    pub(crate) files: usize,
    pub(crate) symbols: usize,
    pub(crate) languages: BTreeMap<&'static str, usize>,
    pub(crate) depends_on: BTreeMap<usize, usize>,
}

/** The repository graph built from a snapshot
 * Fields
    - files: Vec<FileInfo> - aligned with Snapshot::files
    - entities: Vec<Entity> - every symbol, ordered by file then source position
    - modules: Vec<Module> - modules sorted by id
    - folders: Vec<Folder> - folders sorted by path
    - services: Vec<Service> - service candidates sorted by path
    - links: BTreeMap<String, Vec<String>> - call links by location key, stored for the next run
    - relinked: usize - entities whose calls were resolved in this run (the rest reused links)
*/
pub(crate) struct Graph {
    pub(crate) files: Vec<FileInfo>,
    pub(crate) entities: Vec<Entity>,
    pub(crate) modules: Vec<Module>,
    pub(crate) folders: Vec<Folder>,
    pub(crate) services: Vec<Service>,
    pub(crate) links: BTreeMap<String, Vec<String>>,
    pub(crate) relinked: usize,
}

impl Graph {
    /** Return the symbol behind an entity
     * Input
        - snapshot: &'a Snapshot - snapshot the graph was built from
        - entity: &Entity - entity
     * Output
        - &'a Symbol
    */
    pub(crate) fn symbol<'a>(snapshot: &'a Snapshot, entity: &Entity) -> &'a Symbol {
        &snapshot.files[entity.file].outline.symbols[entity.symbol]
    }
}

/** Remove a known extension from a path
 * Input
    - path: &str - file path
 * Output
    - &str path without its last extension
*/
fn without_extension(path: &str) -> &str {
    let name_start = path.rfind('/').map_or(0, |index| index + 1);
    match path[name_start..].rfind('.') {
        Some(dot) if dot > 0 => &path[..name_start + dot],
        _ => path,
    }
}

/** Work out a file's module name from its language's conventions: the declared package for
 * Java and Kotlin, the folder for Go (its import path), the dotted file path for Python, the path
 * without extension for JavaScript and TypeScript, the crate and module path for Rust, and the
 * folder for C and C++ (whose symbols use their namespace instead when they have one)
 * Input
    - file: &SourceFile - file
 * Output
    - String, possibly empty for root-level files
*/
pub(crate) fn module_name(file: &SourceFile) -> String {
    let folder = folder_of(&file.path);
    let language = file.language.map_or("", |language| language.name);
    match language {
        "java" | "kotlin" => file
            .outline
            .package
            .clone()
            .unwrap_or_else(|| folder.replace('/', ".")),
        "go" if folder.is_empty() => file.outline.package.clone().unwrap_or_default(),
        "python" => {
            let path = without_extension(file.path.strip_prefix("src/").unwrap_or(&file.path));
            path.strip_suffix("/__init__")
                .unwrap_or(if path == "__init__" { "" } else { path })
                .replace('/', ".")
        }
        "javascript" | "typescript" => {
            let path = without_extension(file.path.strip_prefix("src/").unwrap_or(&file.path));
            path.strip_suffix("/index")
                .unwrap_or(if path == "index" { "" } else { path })
                .to_string()
        }
        "rust" => {
            let path = without_extension(&file.path);
            let (crate_name, rest) = match path.split_once("src/") {
                Some((before, after)) if before.is_empty() || before.ends_with('/') => {
                    let name = before
                        .trim_end_matches('/')
                        .rsplit('/')
                        .next()
                        .filter(|name| !name.is_empty())
                        .unwrap_or("crate");
                    (name.to_string(), after.to_string())
                }
                _ => (String::new(), path.to_string()),
            };
            let mut parts = rest
                .split('/')
                .filter(|part| !part.is_empty())
                .map(String::from)
                .collect::<Vec<_>>();
            if matches!(
                parts.last().map(String::as_str),
                Some("lib" | "main" | "mod")
            ) {
                parts.pop();
            }
            if !crate_name.is_empty() {
                parts.insert(0, crate_name);
            }
            parts.join(".")
        }
        _ => folder,
    }
}

/** Build the stable semantic id of a symbol: "symbol:" + language + ":" + namespace + "." +
 * qualified name, where the namespace is the C++ namespace when there is one and the module
 * otherwise; ids follow the language's own naming (a Java method keeps its id when its file
 * moves within the package), and collisions are numbered by build_entities
 * Input
    - language: &str - language name
    - module: &str - module name
    - symbol: &Symbol - symbol
 * Output
    - String
*/
pub(crate) fn symbol_id(language: &str, module: &str, symbol: &Symbol) -> String {
    let namespace = symbol.namespace.as_deref().unwrap_or(module);
    if namespace.is_empty() {
        format!("symbol:{language}:{}", symbol.qualified)
    } else {
        format!("symbol:{language}:{namespace}.{}", symbol.qualified)
    }
}

/** Check whether a path is a test file by common conventions: a test folder (test, tests,
 * __tests__, spec, src/test) or a test file name (FooTest, FooTests, test_foo, foo_test,
 * foo.test.*, foo.spec.*)
 * Input
    - path: &str - repository-relative path
 * Output
    - bool
*/
pub(crate) fn is_test_path(path: &str) -> bool {
    let folders = path.split('/').collect::<Vec<_>>();
    let name = folders.last().copied().unwrap_or_default();
    if folders[..folders.len() - 1]
        .iter()
        .any(|folder| matches!(*folder, "test" | "tests" | "__tests__" | "spec" | "testing"))
    {
        return true;
    }
    let stem = without_extension(name);
    stem.ends_with("Test")
        || stem.ends_with("Tests")
        || stem.ends_with("_test")
        || stem.starts_with("test_")
        || stem.ends_with(".test")
        || stem.ends_with(".spec")
        || stem.ends_with("_spec")
}

/** Return the file a test file is named after (FooTest.java tests Foo.java, test_foo.py tests
 * foo.py, foo_test.go tests foo.go, foo.test.ts tests foo.ts), as a bare file name
 * Input
    - path: &str - test file path
 * Output
    - Option<String> the tested file's name
*/
fn tested_name(path: &str) -> Option<String> {
    let name = path.rsplit('/').next()?;
    let extension = &name[without_extension(name).len()..];
    let stem = without_extension(name);
    let base = stem
        .strip_suffix("Tests")
        .or_else(|| stem.strip_suffix("Test"))
        .or_else(|| stem.strip_suffix("_test"))
        .or_else(|| stem.strip_prefix("test_"))
        .or_else(|| stem.strip_suffix(".test"))
        .or_else(|| stem.strip_suffix(".spec"))
        .or_else(|| stem.strip_suffix("_spec"))?;
    (!base.is_empty()).then(|| format!("{base}{extension}"))
}

/** Group languages that call each other directly: JavaScript with TypeScript, C with C++
 * Input
    - language: &str - language name
 * Output
    - &str family name
*/
fn call_family(language: &str) -> &str {
    match language {
        "typescript" => "javascript",
        "c" => "cpp",
        other => other,
    }
}

/** Create the entities of every file, with ids numbered when two symbols share one (overloads),
 * the first in path and line order keeping the bare id
 * Input
    - snapshot: &Snapshot - files and outlines
    - modules: &[usize] - module index per file
    - module_names: &[String] - module name per module index
 * Output
    - Vec<Entity>
*/
fn build_entities(snapshot: &Snapshot, modules: &[usize], module_names: &[String]) -> Vec<Entity> {
    let mut entities = Vec::new();
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (file_index, file) in snapshot.files.iter().enumerate() {
        let Some(language) = file.language else {
            continue;
        };
        let mut occurrences: HashMap<&str, usize> = HashMap::new();
        for (symbol_index, symbol) in file.outline.symbols.iter().enumerate() {
            let module = modules[file_index];
            let base = symbol_id(language.name, &module_names[module], symbol);
            let count = seen.entry(base.clone()).or_insert(0);
            *count += 1;
            let id = if *count == 1 {
                base
            } else {
                format!("{base}~{count}")
            };
            let occurrence = occurrences.entry(&symbol.qualified).or_insert(0);
            let location = format!("{}#{}#{occurrence}", file.path, symbol.qualified);
            *occurrence += 1;
            entities.push(Entity {
                id,
                location,
                file: file_index,
                symbol: symbol_index,
                module,
                callees: Vec::new(),
                callers: Vec::new(),
                ambiguous: Vec::new(),
                tested_by: Vec::new(),
                test: false,
                contracts: Vec::new(),
                risk: Vec::new(),
                score: 0,
            });
        }
    }
    entities
}

/** Resolve one called name for a caller to internal definitions: candidates are callables of
 * the same language family with that name; one in the caller's file wins, then ones in its
 * module, then a single candidate anywhere; anything else is ambiguous (several) or external
 * (none)
 * Input
    - name: &str - called name
    - caller: (usize, usize, &str) - the caller's file, module, and language
    - by_name: &HashMap<&str, Vec<usize>> - callable entities by name
    - entities: &[Entity] - all entities
    - languages: &[&str] - language name per entity
 * Output
    - Result<Vec<usize>, bool> linked entities, or Err(true) when ambiguous and Err(false) when
      external
*/
fn resolve(
    name: &str,
    caller: (usize, usize, &str),
    by_name: &HashMap<&str, Vec<usize>>,
    entities: &[Entity],
    languages: &[&str],
) -> Result<Vec<usize>, bool> {
    let (file, module, language) = caller;
    let candidates = by_name
        .get(name)
        .map(|candidates| {
            candidates
                .iter()
                .copied()
                .filter(|candidate| call_family(languages[*candidate]) == call_family(language))
                .collect::<Vec<_>>()
        })
        .unwrap_or_default();
    if candidates.is_empty() {
        return Err(false);
    }
    let same_file = candidates
        .iter()
        .copied()
        .filter(|candidate| entities[*candidate].file == file)
        .collect::<Vec<_>>();
    if !same_file.is_empty() {
        return Ok(same_file);
    }
    let same_module = candidates
        .iter()
        .copied()
        .filter(|candidate| entities[*candidate].module == module)
        .collect::<Vec<_>>();
    if !same_module.is_empty() {
        return Ok(same_module);
    }
    if candidates.len() == 1 {
        return Ok(candidates);
    }
    Err(true)
}

/** Link every caller to its callees, incrementally when the snapshot carries the previous run's
 * links: resolution of a name depends only on the definitions with that name, so only callers
 * in changed files and callers of a name defined in a changed file (before or after the change)
 * are resolved again, and every other caller reuses its stored links
 * Input
    - snapshot: &Snapshot - files, outlines, and change information
    - entities: &mut [Entity] - entities to link
 * Output
    - (BTreeMap<String, Vec<String>>, usize) the links to store and the number of entities
      resolved in this run
*/
fn link(snapshot: &Snapshot, entities: &mut [Entity]) -> (BTreeMap<String, Vec<String>>, usize) {
    let languages = entities
        .iter()
        .map(|entity| {
            snapshot.files[entity.file]
                .language
                .map_or("", |language| language.name)
        })
        .collect::<Vec<_>>();
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, entity) in entities.iter().enumerate() {
        let symbol = Graph::symbol(snapshot, entity);
        if symbol.kind.callable() {
            by_name.entry(symbol.name.as_str()).or_default().push(index);
        }
    }
    let mut dirty = HashSet::new();
    let outlines = snapshot
        .changed
        .iter()
        .filter_map(|path| snapshot.previous.get(path))
        .map(|outline| outline.as_ref())
        .chain(
            snapshot
                .files
                .iter()
                .filter(|file| snapshot.changed.contains(&file.path))
                .map(|file| file.outline.as_ref()),
        )
        .collect::<Vec<&Outline>>();
    for outline in outlines {
        for symbol in outline
            .symbols
            .iter()
            .filter(|symbol| symbol.kind.callable())
        {
            dirty.insert(symbol.name.clone());
        }
    }
    let locations = entities
        .iter()
        .enumerate()
        .map(|(index, entity)| (entity.location.clone(), index))
        .collect::<HashMap<_, _>>();
    let mut stored = BTreeMap::new();
    let mut relinked = 0;
    for index in 0..entities.len() {
        let symbol = Graph::symbol(snapshot, &entities[index]);
        if !symbol.kind.callable() {
            continue;
        }
        let path = &snapshot.files[entities[index].file].path;
        let previous = snapshot.links.as_ref().and_then(|links| {
            if snapshot.changed.contains(path)
                || symbol.calls.iter().any(|name| dirty.contains(name))
            {
                return None;
            }
            let targets = links.get(&entities[index].location)?;
            let mut callees = Vec::new();
            let mut ambiguous = Vec::new();
            for target in targets {
                match target.strip_prefix('?') {
                    Some(name) => ambiguous.push(name.to_string()),
                    None => callees.push(*locations.get(target)?),
                }
            }
            Some((callees, ambiguous))
        });
        let (callees, ambiguous) = match previous {
            Some(found) => found,
            None => {
                relinked += 1;
                let caller = (
                    entities[index].file,
                    entities[index].module,
                    languages[index],
                );
                let mut callees = BTreeSet::new();
                let mut ambiguous = Vec::new();
                for name in &symbol.calls {
                    match resolve(name, caller, &by_name, entities, &languages) {
                        Ok(found) => {
                            callees.extend(found.into_iter().filter(|target| *target != index))
                        }
                        Err(true) => ambiguous.push(name.clone()),
                        Err(false) => {}
                    }
                }
                (callees.into_iter().collect(), ambiguous)
            }
        };
        let mut targets = callees
            .iter()
            .map(|callee: &usize| entities[*callee].location.clone())
            .collect::<Vec<_>>();
        targets.extend(ambiguous.iter().map(|name| format!("?{name}")));
        stored.insert(entities[index].location.clone(), targets);
        entities[index].callees = callees;
        entities[index].ambiguous = ambiguous;
    }
    for index in 0..entities.len() {
        for callee in entities[index].callees.clone() {
            entities[callee].callers.push(index);
        }
    }
    for entity in entities.iter_mut() {
        entity.callers.sort_unstable();
        entity.callers.dedup();
    }
    (stored, relinked)
}

/** Find the service candidates: folders holding a manifest, plus children of conventional
 * service folders (services/, apps/, cmd/, packages/), or the repository root when nothing else
 * qualifies
 * Input
    - snapshot: &Snapshot - files
 * Output
    - Vec<Service> sorted by path, with no file counts yet
*/
fn find_services(snapshot: &Snapshot) -> Vec<Service> {
    let mut evidence: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for file in &snapshot.files {
        let name = file.path.rsplit('/').next().unwrap_or(&file.path);
        if MANIFESTS.contains(&name) || name.ends_with(".csproj") {
            evidence
                .entry(folder_of(&file.path))
                .or_default()
                .push(name.to_string());
        }
        let parts = file.path.split('/').collect::<Vec<_>>();
        if parts.len() > 2 && SERVICE_LAYOUTS.contains(&parts[0]) {
            let folder = format!("{}/{}", parts[0], parts[1]);
            let entry = evidence.entry(folder).or_default();
            let layout = format!("layout:{}/", parts[0]);
            if !entry.contains(&layout) {
                entry.push(layout);
            }
        }
    }
    evidence
        .entry(String::new())
        .or_insert_with(|| vec!["repository root".into()]);
    evidence
        .into_iter()
        .map(|(path, mut evidence)| {
            evidence.sort();
            evidence.dedup();
            Service {
                id: format!("service:{}", if path.is_empty() { "." } else { &path }),
                path,
                evidence,
                files: 0,
                symbols: 0,
                languages: BTreeMap::new(),
                depends_on: BTreeMap::new(),
            }
        })
        .collect()
}

/** Find the service a path belongs to: the deepest service folder containing it
 * Input
    - services: &[Service] - service candidates
    - path: &str - file path
 * Output
    - usize index into services
*/
fn service_of(services: &[Service], path: &str) -> usize {
    services
        .iter()
        .enumerate()
        .filter(|(_, service)| {
            service.path.is_empty() || path.starts_with(&folder_prefix(&service.path))
        })
        .max_by_key(|(_, service)| service.path.len())
        .map_or(0, |(index, _)| index)
}

/** Build the repository graph from a snapshot: modules and services per file, entities with
 * stable ids, call links (incremental), test relationships, folder and service rollups
 * Input
    - snapshot: &Snapshot - files and outlines
 * Output
    - Graph
*/
pub(crate) fn build(snapshot: &Snapshot) -> Graph {
    let mut services = find_services(snapshot);
    let mut module_index: BTreeMap<(&'static str, String), usize> = BTreeMap::new();
    let mut file_modules = Vec::with_capacity(snapshot.files.len());
    let mut files = Vec::with_capacity(snapshot.files.len());
    for file in &snapshot.files {
        let service = service_of(&services, &file.path);
        let module = match file.language {
            Some(language) => {
                let next = module_index.len();
                *module_index
                    .entry((language.name, module_name(file)))
                    .or_insert(next)
            }
            None => usize::MAX,
        };
        file_modules.push(module);
        files.push(FileInfo {
            module,
            service,
            owners: Vec::new(),
            test: is_test_path(&file.path),
            tests: BTreeSet::new(),
            tested_by: BTreeSet::new(),
        });
    }
    // Number modules in id order so the output is deterministic
    let mut ordered = module_index.into_iter().collect::<Vec<_>>();
    ordered.sort_by(|left, right| left.0.cmp(&right.0));
    let mut renumber = vec![0; ordered.len()];
    let mut modules = Vec::with_capacity(ordered.len());
    for (position, ((language, name), old)) in ordered.into_iter().enumerate() {
        renumber[old] = position;
        modules.push(Module {
            id: format!(
                "module:{language}:{}",
                if name.is_empty() { "(root)" } else { &name }
            ),
            language,
            name,
            files: 0,
            symbols: 0,
            service: 0,
            depends_on: BTreeMap::new(),
            imports: BTreeSet::new(),
        });
    }
    for (info, module) in files.iter_mut().zip(file_modules.iter_mut()) {
        if *module != usize::MAX {
            *module = renumber[*module];
            info.module = *module;
        }
    }
    let names = modules
        .iter()
        .map(|module| module.name.clone())
        .collect::<Vec<_>>();
    let mut entities = build_entities(snapshot, &file_modules, &names);
    let (links, relinked) = link(snapshot, &mut entities);

    // Tests: test symbols and callables in test files exercise what they call
    for entity in entities.iter_mut() {
        let symbol = Graph::symbol(snapshot, entity);
        entity.test = symbol.test || (files[entity.file].test && symbol.kind.callable());
    }
    for index in 0..entities.len() {
        if !entities[index].test {
            continue;
        }
        let test_id = entities[index].id.clone();
        let test_file = entities[index].file;
        for callee in entities[index].callees.clone() {
            if !entities[callee].test {
                entities[callee].tested_by.push(test_id.clone());
                let target_file = entities[callee].file;
                files[test_file].tests.insert(target_file);
                files[target_file].tested_by.insert(test_file);
            }
        }
    }
    file_level_tests(snapshot, &mut files, &mut entities);

    // Rollups: modules, services, folders
    let mut folders: BTreeMap<String, Folder> = BTreeMap::new();
    for (index, file) in snapshot.files.iter().enumerate() {
        let info = &files[index];
        let symbols = file.outline.symbols.len();
        if info.module != usize::MAX {
            let module = &mut modules[info.module];
            if module.files == 0 {
                module.service = info.service;
            }
            module.files += 1;
            module.symbols += symbols;
            module.imports.extend(file.outline.imports.iter().cloned());
        }
        let service = &mut services[info.service];
        service.files += 1;
        service.symbols += symbols;
        if let Some(language) = file.language {
            *service.languages.entry(language.name).or_default() += 1;
        }
        let mut folder = folder_of(&file.path);
        let direct = folder.clone();
        loop {
            let entry = folders.entry(folder.clone()).or_insert_with(|| Folder {
                path: folder.clone(),
                files: 0,
                files_total: 0,
                symbols_total: 0,
                children: BTreeSet::new(),
                depends_on: BTreeMap::new(),
            });
            entry.files_total += 1;
            entry.symbols_total += symbols;
            if folder == direct {
                entry.files += 1;
            }
            if folder.is_empty() {
                break;
            }
            let parent = folder_of(&folder);
            folders
                .entry(parent.clone())
                .or_insert_with(|| Folder {
                    path: parent.clone(),
                    files: 0,
                    files_total: 0,
                    symbols_total: 0,
                    children: BTreeSet::new(),
                    depends_on: BTreeMap::new(),
                })
                .children
                .insert(folder.clone());
            folder = parent;
        }
    }
    for entity in &entities {
        for callee in &entity.callees {
            let target = &entities[*callee];
            if target.module != entity.module {
                *modules[entity.module]
                    .depends_on
                    .entry(target.module)
                    .or_default() += 1;
            }
            let (from, to) = (files[entity.file].service, files[target.file].service);
            if from != to {
                *services[from].depends_on.entry(to).or_default() += 1;
            }
            let (from, to) = (
                folder_of(&snapshot.files[entity.file].path),
                folder_of(&snapshot.files[target.file].path),
            );
            if from != to {
                if let Some(folder) = folders.get_mut(&from) {
                    *folder.depends_on.entry(to).or_default() += 1;
                }
            }
        }
    }
    Graph {
        files,
        entities,
        modules,
        folders: folders.into_values().collect(),
        services,
        links,
        relinked,
    }
}

/** Link test files to the code their file-level calls exercise (test frameworks such as Jest or
 * pytest often call code from anonymous callbacks), and pair test files with the files they are
 * named after (FooTest.java and Foo.java)
 * Input
    - snapshot: &Snapshot - files and outlines
    - files: &mut [FileInfo] - file information, updated
    - entities: &mut [Entity] - entities, updated with "file:PATH" test links
 * Output
    - None
*/
fn file_level_tests(snapshot: &Snapshot, files: &mut [FileInfo], entities: &mut [Entity]) {
    let mut by_name: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut by_file_name: HashMap<&str, Vec<usize>> = HashMap::new();
    for (index, entity) in entities.iter().enumerate() {
        let symbol = Graph::symbol(snapshot, entity);
        if symbol.kind.callable() && !entity.test {
            by_name.entry(symbol.name.as_str()).or_default().push(index);
        }
    }
    let mut per_file: HashMap<usize, Vec<usize>> = HashMap::new();
    for (index, entity) in entities.iter().enumerate() {
        per_file.entry(entity.file).or_default().push(index);
    }
    for (index, file) in snapshot.files.iter().enumerate() {
        if !files[index].test {
            let name = file.path.rsplit('/').next().unwrap_or(&file.path);
            by_file_name.entry(name).or_default().push(index);
        }
    }
    for test_file in 0..snapshot.files.len() {
        if !files[test_file].test {
            continue;
        }
        let file = &snapshot.files[test_file];
        let language = file.language.map_or("", |language| language.name);
        let reached = per_file
            .get(&test_file)
            .into_iter()
            .flatten()
            .flat_map(|entity| entities[*entity].callees.iter().copied())
            .collect::<HashSet<_>>();
        let marker = format!("file:{}", file.path);
        for name in &file.outline.calls {
            let candidates = by_name
                .get(name.as_str())
                .map(|candidates| {
                    candidates
                        .iter()
                        .copied()
                        .filter(|candidate| {
                            let other = snapshot.files[entities[*candidate].file]
                                .language
                                .map_or("", |language| language.name);
                            call_family(other) == call_family(language)
                        })
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default();
            let same_module = candidates
                .iter()
                .copied()
                .filter(|candidate| entities[*candidate].module == files[test_file].module)
                .collect::<Vec<_>>();
            let chosen = if !same_module.is_empty() {
                same_module
            } else if candidates.len() == 1 {
                candidates
            } else {
                Vec::new()
            };
            for target in chosen {
                let target_file = entities[target].file;
                files[test_file].tests.insert(target_file);
                files[target_file].tested_by.insert(test_file);
                if !reached.contains(&target) && !entities[target].tested_by.contains(&marker) {
                    entities[target].tested_by.push(marker.clone());
                }
            }
        }
        if let Some(name) = tested_name(&file.path) {
            for target_file in by_file_name.get(name.as_str()).cloned().unwrap_or_default() {
                files[test_file].tests.insert(target_file);
                files[target_file].tested_by.insert(test_file);
            }
        }
    }
}
