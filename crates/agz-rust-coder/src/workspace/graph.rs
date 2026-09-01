use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::path::PathBuf;

use cargo_metadata::{DependencyKind, Metadata};

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PackageNode {
    pub package_id: String,
    pub name: String,
    pub version: String,
    pub manifest_path: PathBuf,
    pub root: PathBuf,
    pub workspace_member: bool,
    pub external_path: bool,
    pub enabled_features: Vec<String>,
}

#[derive(Debug, Clone, Eq, PartialEq)]
pub struct PackageEdge {
    pub from_package_id: String,
    pub to_package_id: String,
    pub dependency_name: String,
    pub kinds: Vec<DependencyKind>,
}

#[derive(Debug, Clone, Default, Eq, PartialEq)]
pub struct PackageGraph {
    nodes: BTreeMap<String, PackageNode>,
    outgoing: BTreeMap<String, Vec<PackageEdge>>,
    incoming: BTreeMap<String, Vec<PackageEdge>>,
}

impl PackageGraph {
    pub fn nodes(&self) -> &BTreeMap<String, PackageNode> {
        &self.nodes
    }

    pub fn node(&self, package_id: &str) -> Option<&PackageNode> {
        self.nodes.get(package_id)
    }

    pub fn outgoing(&self, package_id: &str) -> &[PackageEdge] {
        self.outgoing.get(package_id).map_or(&[], Vec::as_slice)
    }

    pub fn incoming(&self, package_id: &str) -> &[PackageEdge] {
        self.incoming.get(package_id).map_or(&[], Vec::as_slice)
    }

    pub fn external_path_roots(&self) -> impl Iterator<Item = &PathBuf> {
        self.nodes
            .values()
            .filter(|node| node.external_path)
            .map(|node| &node.root)
    }

    pub fn reverse_dependents<I, S>(&self, seeds: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = S>,
        S: Into<String>,
    {
        let mut affected = BTreeSet::new();
        let mut queue = VecDeque::new();
        for seed in seeds {
            let seed = seed.into();
            if affected.insert(seed.clone()) {
                queue.push_back(seed);
            }
        }
        while let Some(package_id) = queue.pop_front() {
            for edge in self.incoming(&package_id) {
                if affected.insert(edge.from_package_id.clone()) {
                    queue.push_back(edge.from_package_id.clone());
                }
            }
        }
        affected
    }

    pub fn affected_by_paths<'a, I>(&self, paths: I) -> BTreeSet<String>
    where
        I: IntoIterator<Item = &'a std::path::Path>,
    {
        let paths: Vec<PathBuf> = paths.into_iter().map(PathBuf::from).collect();
        let seeds = self
            .nodes
            .values()
            .filter(|node| {
                paths.iter().any(|path| {
                    path.as_path() == node.root.as_path() || path.starts_with(&node.root)
                })
            })
            .map(|node| node.package_id.clone());
        self.reverse_dependents(seeds)
    }
}

pub fn build_package_graph(metadata: &Metadata) -> PackageGraph {
    let workspace_members: BTreeSet<String> = metadata
        .workspace_members
        .iter()
        .map(|package_id| package_id.repr.clone())
        .collect();
    let mut graph = PackageGraph::default();

    for package in &metadata.packages {
        let manifest_path = PathBuf::from(package.manifest_path.as_std_path());
        let root = manifest_path
            .parent()
            .map_or_else(|| manifest_path.clone(), PathBuf::from);
        let package_id = package.id.repr.clone();
        let workspace_member = workspace_members.contains(&package_id);
        graph.nodes.insert(
            package_id.clone(),
            PackageNode {
                package_id,
                name: package.name.as_ref().to_owned(),
                version: package.version.to_string(),
                manifest_path,
                root,
                workspace_member,
                external_path: package.source.is_none() && !workspace_member,
                enabled_features: Vec::new(),
            },
        );
        if let Some(resolve) = metadata.resolve.as_ref()
            && let Some(node) = resolve.nodes.iter().find(|node| node.id == package.id)
        {
            let enabled_features = node
                .features
                .iter()
                .map(|feature| feature.as_ref().to_owned())
                .collect();
            if let Some(package_node) = graph.nodes.get_mut(&package.id.repr) {
                package_node.enabled_features = enabled_features;
            }
        }
    }

    if let Some(resolve) = metadata.resolve.as_ref() {
        for node in &resolve.nodes {
            if !graph.nodes.contains_key(&node.id.repr) {
                continue;
            }
            if node.deps.is_empty() {
                for dependency in &node.dependencies {
                    let dependency_name = graph
                        .node(&dependency.repr)
                        .map(|package| package.name.clone())
                        .unwrap_or_default();
                    add_edge(
                        &mut graph,
                        PackageEdge {
                            from_package_id: node.id.repr.clone(),
                            to_package_id: dependency.repr.clone(),
                            dependency_name,
                            kinds: vec![DependencyKind::Normal],
                        },
                    );
                }
            } else {
                for dependency in &node.deps {
                    add_edge(
                        &mut graph,
                        PackageEdge {
                            from_package_id: node.id.repr.clone(),
                            to_package_id: dependency.pkg.repr.clone(),
                            dependency_name: dependency.name.clone(),
                            kinds: sorted_dependency_kinds(
                                dependency.dep_kinds.iter().map(|kind| kind.kind),
                            ),
                        },
                    );
                }
            }
        }
    }

    for edges in graph.outgoing.values_mut() {
        edges.sort_by(|left, right| {
            left.to_package_id
                .cmp(&right.to_package_id)
                .then_with(|| left.dependency_name.cmp(&right.dependency_name))
                .then_with(|| compare_dependency_kinds(&left.kinds, &right.kinds))
        });
    }
    for edges in graph.incoming.values_mut() {
        edges.sort_by(|left, right| {
            left.from_package_id
                .cmp(&right.from_package_id)
                .then_with(|| left.dependency_name.cmp(&right.dependency_name))
                .then_with(|| compare_dependency_kinds(&left.kinds, &right.kinds))
        });
    }
    graph
}

fn sorted_dependency_kinds<I>(kinds: I) -> Vec<DependencyKind>
where
    I: IntoIterator<Item = DependencyKind>,
{
    let mut kinds = kinds.into_iter().collect::<Vec<_>>();
    kinds.sort_by_key(|kind| dependency_kind_rank(*kind));
    kinds.dedup();
    kinds
}

fn compare_dependency_kinds(
    left: &[DependencyKind],
    right: &[DependencyKind],
) -> std::cmp::Ordering {
    left.iter()
        .map(|kind| dependency_kind_rank(*kind))
        .cmp(right.iter().map(|kind| dependency_kind_rank(*kind)))
}

fn dependency_kind_rank(kind: DependencyKind) -> u8 {
    match kind {
        DependencyKind::Normal => 0,
        DependencyKind::Development => 1,
        DependencyKind::Build => 2,
        DependencyKind::Unknown => 3,
    }
}

fn add_edge(graph: &mut PackageGraph, edge: PackageEdge) {
    if !graph.nodes.contains_key(&edge.to_package_id) {
        return;
    }
    if graph
        .outgoing
        .get(&edge.from_package_id)
        .is_some_and(|edges| edges.contains(&edge))
    {
        return;
    }
    graph
        .outgoing
        .entry(edge.from_package_id.clone())
        .or_default()
        .push(edge.clone());
    graph
        .incoming
        .entry(edge.to_package_id.clone())
        .or_default()
        .push(edge);
}
