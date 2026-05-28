#!/usr/bin/env python3
"""Rebuild graphify output with scoped IDs, cross-crate edges, test filtering,
auto-labeled communities, and an agent-optimized report."""
from __future__ import annotations

import argparse
import json
import re
import shutil
import sys
import tempfile
from collections import Counter, defaultdict
from dataclasses import dataclass
from pathlib import Path
from typing import Iterable

import networkx as nx
from networkx.readwrite import json_graph

from graphify.analyze import god_nodes, suggest_questions, surprising_connections
from graphify.cluster import cluster, score_all
from graphify.detect import detect
from graphify.extract import extract
from graphify.report import generate


LOCAL_RELATIONS = {
    "calls",
    "contains",
    "inherits",
    "implements",
    "defines",
    "declares",
    "rationale_for",
    "tests",
}

# Relations injected by the cross-crate edge pipeline.  These are explicitly
# non-local and must NOT trigger the same-file validation check.
CROSS_CRATE_RELATIONS = {
    "uses",
    "depends_on",
    "contains_entity",
    "crate_depends_on",
}


@dataclass(frozen=True)
class BuildArtifacts:
    graph_json: Path
    report_md: Path
    metadata_json: Path


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def make_scoped_id(source_file: str, old_id: str) -> str:
    path_part = source_file.replace("\\", "/").replace("/", "__").replace(".", "_").replace("-", "_")
    return f"{path_part}__{old_id}".lower()


def _parse_line_number(source_location: str | None) -> int | None:
    """Extract the line number from a source_location like 'L42'."""
    if not source_location:
        return None
    m = re.match(r"L(\d+)", source_location)
    return int(m.group(1)) if m else None


# ---------------------------------------------------------------------------
# Phase 1: Tag Test Nodes
# ---------------------------------------------------------------------------

def _build_test_line_ranges(rs_path: Path) -> set[int]:
    """Return a set of line numbers that fall inside #[cfg(test)] modules or
    are #[test] functions.  Uses simple brace-depth tracking."""
    try:
        lines = rs_path.read_text(errors="replace").splitlines()
    except (OSError, UnicodeDecodeError):
        return set()

    test_lines: set[int] = set()
    in_test_mod = False
    brace_depth = 0
    test_mod_start_depth = 0

    for i, line in enumerate(lines, start=1):
        stripped = line.strip()

        # Track brace depth
        brace_depth += stripped.count("{") - stripped.count("}")

        if not in_test_mod:
            if "#[cfg(test)]" in stripped:
                in_test_mod = True
                test_mod_start_depth = brace_depth
                test_lines.add(i)
        else:
            test_lines.add(i)
            if brace_depth <= test_mod_start_depth and "}" in stripped:
                in_test_mod = False

        # Individual #[test] functions (outside cfg(test) blocks)
        if not in_test_mod and stripped == "#[test]":
            test_lines.add(i)
            # Tag the next few lines as test (the fn signature + body start)
            for j in range(i + 1, min(i + 4, len(lines) + 1)):
                test_lines.add(j)

    return test_lines


def tag_test_nodes(extraction: dict, root: Path) -> dict:
    """Annotate extraction nodes with ``is_test: true`` when they originate
    from ``#[cfg(test)]`` blocks, ``#[test]`` functions, or ``tests/`` dirs."""

    # Build per-file test line sets (lazily, only for files that appear)
    file_test_cache: dict[str, set[int]] = {}

    def is_test_location(source_file: str | None, source_location: str | None) -> bool:
        if not source_file:
            return False
        # Anything under a tests/ directory is a test
        if "/tests/" in source_file or source_file.startswith("tests/"):
            return True
        if source_file not in file_test_cache:
            full_path = root / source_file
            if full_path.suffix == ".rs" and full_path.exists():
                file_test_cache[source_file] = _build_test_line_ranges(full_path)
            else:
                file_test_cache[source_file] = set()
        line = _parse_line_number(source_location)
        if line is None:
            return False
        return line in file_test_cache[source_file]

    tagged = dict(extraction)

    # Tag nodes
    test_node_ids: set[str] = set()
    new_nodes = []
    for node in extraction.get("nodes", []):
        node = dict(node)
        if is_test_location(node.get("source_file"), node.get("source_location")):
            node["is_test"] = True
            test_node_ids.add(node["id"])
        new_nodes.append(node)
    tagged["nodes"] = new_nodes

    # Tag edges where both endpoints are test nodes
    new_edges = []
    for edge in extraction.get("edges", []):
        edge = dict(edge)
        if edge.get("source") in test_node_ids and edge.get("target") in test_node_ids:
            edge["is_test"] = True
        new_edges.append(edge)
    tagged["edges"] = new_edges

    return tagged


# ---------------------------------------------------------------------------
# Phase 2: Inject Cross-Crate Edges
# ---------------------------------------------------------------------------

def _parse_workspace_crate_deps(root: Path) -> tuple[list[str], dict[str, list[str]]]:
    """Parse workspace Cargo.toml for members and each crate's Cargo.toml for
    inter-crate dependencies.  Returns (crate_names, crate_deps)."""
    workspace_toml = root / "Cargo.toml"
    if not workspace_toml.exists():
        return [], {}

    text = workspace_toml.read_text()
    # Extract members list
    members: list[str] = re.findall(r'"(crates/[^"]+)"', text)
    crate_names = [m.split("/")[-1] for m in members]

    crate_deps: dict[str, list[str]] = {}
    for member_path in members:
        crate_name = member_path.split("/")[-1]
        crate_toml = root / member_path / "Cargo.toml"
        if not crate_toml.exists():
            continue
        crate_text = crate_toml.read_text()
        # Find path dependencies pointing to sibling phantom crates
        dep_names = re.findall(r'(phantom-[\w-]+)\s*=\s*\{[^}]*path\s*=', crate_text)
        # Filter to workspace members only
        deps = [d for d in dep_names if d in crate_names]
        if deps:
            crate_deps[crate_name] = deps

    return crate_names, crate_deps


def _scan_use_imports(root: Path) -> list[dict]:
    """Scan all .rs files for ``use phantom_*::`` imports.  Returns a list of
    dicts with source_file, target_crate, imported_items."""
    imports: list[dict] = []
    crates_dir = root / "crates"
    if not crates_dir.exists():
        return imports

    # Match: use phantom_foo::bar::Baz;  or  use phantom_foo::{A, B};
    use_re = re.compile(r"use\s+(phantom_\w+)::(.+?);")

    for rs_file in crates_dir.rglob("*.rs"):
        try:
            text = rs_file.read_text(errors="replace")
        except OSError:
            continue
        rel_path = str(rs_file.relative_to(root))
        for m in use_re.finditer(text):
            crate_mod = m.group(1)  # e.g. phantom_agents
            import_path = m.group(2).strip()  # e.g. agent::PauseReason or {A, B}
            target_crate = crate_mod.replace("_", "-")

            # Extract individual items from the import path
            if import_path.startswith("{"):
                items = [i.strip().split("::")[-1] for i in import_path.strip("{}").split(",")]
            else:
                items = [import_path.split("::")[-1].strip()]
            # Clean up items (remove aliases, wildcards)
            items = [i.split(" as ")[0].strip() for i in items if i.strip() and i.strip() != "*"]

            for item in items:
                imports.append({
                    "source_file": rel_path,
                    "target_crate": target_crate,
                    "item": item,
                })

    return imports


def inject_cross_crate_edges(
    extraction: dict, root: Path
) -> tuple[dict, dict[str, list[str]]]:
    """Add cross-crate ``uses`` edges from ``use phantom_*`` import analysis."""
    crate_names, crate_deps = _parse_workspace_crate_deps(root)
    imports = _scan_use_imports(root)

    if not imports:
        return extraction, crate_deps

    # Build a lookup: (crate_name, label) -> node_id for resolution
    label_to_nid: dict[tuple[str, str], str] = {}
    for node in extraction.get("nodes", []):
        sf = node.get("source_file", "")
        label = node.get("label", "")
        if not sf or not label:
            continue
        # Determine which crate this node belongs to
        parts = sf.split("/")
        if len(parts) >= 2 and parts[0] == "crates":
            crate = parts[1]
            key = (crate, label)
            if key not in label_to_nid:
                label_to_nid[key] = node["id"]

    # Build file-node lookup: source_file -> node_id (for file-level nodes)
    file_to_nid: dict[str, str] = {}
    for node in extraction.get("nodes", []):
        sf = node.get("source_file", "")
        if sf and node.get("file_type") == "code":
            # File-level nodes typically have label == filename
            if node.get("label", "").endswith(".rs"):
                file_to_nid[sf] = node["id"]

    # Source file -> its node ID (the file-level node of the importing file)
    import_source_nids: dict[str, str] = {}
    for node in extraction.get("nodes", []):
        sf = node.get("source_file", "")
        if sf and node.get("label", "").endswith(".rs"):
            import_source_nids[sf] = node["id"]

    new_edges = list(extraction.get("edges", []))
    edges_added = 0

    for imp in imports:
        source_file = imp["source_file"]
        target_crate = imp["target_crate"]
        item = imp["item"]

        # Find source node (file-level node of the importing file)
        source_nid = import_source_nids.get(source_file)
        if not source_nid:
            continue

        # Try to resolve target to a specific entity node
        target_nid = label_to_nid.get((target_crate, item))

        if target_nid:
            new_edges.append({
                "source": source_nid,
                "target": target_nid,
                "relation": "uses",
                "confidence": "INFERRED",
                "weight": 2.0,
                "cross_crate": True,
                "source_file": source_file,
                "source_location": "L0",
            })
            edges_added += 1
        else:
            # Fall back to lib.rs node of the target crate
            lib_file = f"crates/{target_crate}/src/lib.rs"
            fallback_nid = file_to_nid.get(lib_file)
            if fallback_nid:
                new_edges.append({
                    "source": source_nid,
                    "target": fallback_nid,
                    "relation": "depends_on",
                    "confidence": "INFERRED",
                    "weight": 1.5,
                    "cross_crate": True,
                    "source_file": source_file,
                    "source_location": "L0",
                })
                edges_added += 1

    result = dict(extraction)
    result["edges"] = new_edges
    print(f"  Cross-crate edges injected: {edges_added}")
    return result, crate_deps


# ---------------------------------------------------------------------------
# Phase 3: Crate Summary Nodes
# ---------------------------------------------------------------------------

def add_crate_summary_nodes(
    graph: nx.DiGraph, root: Path, crate_deps: dict[str, list[str]]
) -> None:
    """Add synthetic ``crate:NAME`` hub nodes and crate-to-crate dependency edges."""
    crates_dir = root / "crates"
    if not crates_dir.exists():
        return

    crate_dirs = sorted(
        d.name for d in crates_dir.iterdir()
        if d.is_dir() and (d / "Cargo.toml").exists()
    )

    # Map existing nodes to their crate
    crate_members: dict[str, list[str]] = defaultdict(list)
    for nid, attrs in graph.nodes(data=True):
        sf = attrs.get("source_file", "")
        parts = sf.split("/") if sf else []
        if len(parts) >= 2 and parts[0] == "crates":
            crate_members[parts[1]].append(nid)

    for crate_name in crate_dirs:
        crate_nid = f"crate:{crate_name}"
        graph.add_node(
            crate_nid,
            label=crate_nid,
            file_type="crate",
            source_file=f"crates/{crate_name}/Cargo.toml",
            source_location="L1",
            is_synthetic=True,
        )
        # Connect to member entities (low weight so it doesn't dominate clustering)
        for member_nid in crate_members.get(crate_name, []):
            graph.add_edge(
                crate_nid, member_nid,
                relation="contains_entity",
                weight=0.5,
                _src=crate_nid,
                _tgt=member_nid,
            )

    # Crate-to-crate dependency edges (high weight = architectural backbone)
    for crate_name, deps in crate_deps.items():
        src = f"crate:{crate_name}"
        if src not in graph:
            continue
        for dep in deps:
            tgt = f"crate:{dep}"
            if tgt not in graph:
                continue
            graph.add_edge(
                src, tgt,
                relation="crate_depends_on",
                weight=3.0,
                _src=src,
                _tgt=tgt,
            )


# ---------------------------------------------------------------------------
# Phase 4: Filter Test Nodes for Clustering
# ---------------------------------------------------------------------------

def build_clustering_subgraph(graph: nx.DiGraph) -> nx.DiGraph:
    """Return a subgraph excluding test and synthetic nodes for cleaner
    community detection."""
    keep = [
        nid for nid, attrs in graph.nodes(data=True)
        if not attrs.get("is_test") and not attrs.get("is_synthetic")
    ]
    return graph.subgraph(keep).copy()


def assign_excluded_nodes(
    full_graph: nx.DiGraph,
    clustering_graph: nx.DiGraph,
    communities: dict[int, list[str]],
) -> dict[int, list[str]]:
    """Assign test and synthetic nodes to the nearest community."""
    community_by_node = node_community_map(communities)
    clustered = set(clustering_graph.nodes)

    for nid in full_graph.nodes:
        if nid in clustered:
            continue

        # Find the community of the most-connected non-excluded neighbor
        neighbor_communities: Counter[int] = Counter()
        for neighbor in full_graph.predecessors(nid):
            if neighbor in community_by_node:
                neighbor_communities[community_by_node[neighbor]] += 1
        for neighbor in full_graph.successors(nid):
            if neighbor in community_by_node:
                neighbor_communities[community_by_node[neighbor]] += 1

        if neighbor_communities:
            best = neighbor_communities.most_common(1)[0][0]
        else:
            best = 0  # default community

        community_by_node[nid] = best
        communities.setdefault(best, []).append(nid)

    return communities


# ---------------------------------------------------------------------------
# Phase 5: Auto-Label Communities
# ---------------------------------------------------------------------------

def label_communities(
    graph: nx.DiGraph, communities: dict[int, list[str]]
) -> dict[int, str]:
    """Generate semantic labels for communities based on dominant crate and
    highest-degree non-test entity."""
    labels: dict[int, str] = {}

    for cid, members in communities.items():
        # Count crate membership
        crate_counts: Counter[str] = Counter()
        for nid in members:
            attrs = graph.nodes.get(nid, {})
            if attrs.get("is_test") or attrs.get("is_synthetic"):
                continue
            sf = attrs.get("source_file", "")
            parts = sf.split("/") if sf else []
            if len(parts) >= 2 and parts[0] == "crates":
                crate_counts[parts[1]] += 1

        if not crate_counts:
            labels[cid] = f"Community {cid}"
            continue

        # Find dominant crate(s)
        total = sum(crate_counts.values())
        top_crates = crate_counts.most_common(3)
        dominant = top_crates[0][0]
        dominant_pct = top_crates[0][1] / total if total else 0

        # Find keyword: highest-degree non-test struct/trait (no parens in label)
        keyword = ""
        best_degree = -1
        for nid in members:
            attrs = graph.nodes.get(nid, {})
            if attrs.get("is_test") or attrs.get("is_synthetic"):
                continue
            lbl = attrs.get("label", "")
            # Prefer structs/enums/traits (no parentheses = not a function)
            if "(" in lbl or ")" in lbl or lbl.endswith(".rs"):
                continue
            deg = graph.degree(nid)
            if deg > best_degree:
                best_degree = deg
                keyword = lbl

        if not keyword:
            # Fall back to highest-degree function
            for nid in members:
                attrs = graph.nodes.get(nid, {})
                if attrs.get("is_test") or attrs.get("is_synthetic"):
                    continue
                lbl = attrs.get("label", "")
                if lbl.endswith(".rs"):
                    continue
                deg = graph.degree(nid)
                if deg > best_degree:
                    best_degree = deg
                    keyword = lbl

        # Build label
        if dominant_pct >= 0.8 or len(top_crates) == 1:
            crate_part = dominant
        elif len(top_crates) >= 2 and top_crates[1][1] / total >= 0.2:
            crate_part = f"{top_crates[0][0]}, {top_crates[1][0]}"
        else:
            crate_part = dominant

        if keyword:
            labels[cid] = f"{keyword} ({crate_part})"
        else:
            labels[cid] = f"{crate_part}"

    return labels


# ---------------------------------------------------------------------------
# Phase 6: Prune Noise
# ---------------------------------------------------------------------------

def prune_noise(
    graph: nx.DiGraph, communities: dict[int, list[str]]
) -> tuple[nx.DiGraph, dict[int, list[str]]]:
    """Remove isolated nodes and merge thin communities."""
    community_by_node = node_community_map(communities)

    # Remove degree-0 nodes (truly isolated)
    isolates = [nid for nid in graph.nodes if graph.degree(nid) == 0]
    for nid in isolates:
        cid = community_by_node.get(nid)
        if cid is not None and nid in communities.get(cid, []):
            communities[cid].remove(nid)
        graph.remove_node(nid)
    if isolates:
        print(f"  Pruned {len(isolates)} isolated nodes")

    # Deduplicate: if both imports_from and uses exist between same pair, drop imports_from
    edges_to_remove = []
    for u, v, data in graph.edges(data=True):
        if data.get("relation") == "imports_from":
            # Check if a 'uses' edge exists for the same pair
            if graph.has_edge(u, v):
                for _, edata in graph[u][v].items() if graph.is_multigraph() else [(0, graph[u][v])]:
                    if isinstance(edata, dict) and edata.get("relation") == "uses":
                        edges_to_remove.append((u, v, "imports_from"))
                        break
    for u, v, _rel in edges_to_remove:
        # Only remove if the edge is still imports_from
        if graph.has_edge(u, v) and graph[u][v].get("relation") == "imports_from":
            graph.remove_edge(u, v)
    if edges_to_remove:
        print(f"  Deduplicated {len(edges_to_remove)} redundant imports_from edges")

    # Merge thin communities (<3 non-test nodes) into nearest neighbor
    community_by_node = node_community_map(communities)  # refresh
    thin_cids = []
    for cid, members in communities.items():
        non_test = [m for m in members if not graph.nodes.get(m, {}).get("is_test")]
        if len(non_test) < 3:
            thin_cids.append(cid)

    merged_count = 0
    for cid in thin_cids:
        members = communities.get(cid, [])
        if not members:
            continue
        # Find the most-connected neighbor community
        neighbor_cids: Counter[int] = Counter()
        for nid in members:
            for neighbor in list(graph.predecessors(nid)) + list(graph.successors(nid)):
                ncid = community_by_node.get(neighbor)
                if ncid is not None and ncid != cid and ncid not in thin_cids:
                    neighbor_cids[ncid] += 1

        if not neighbor_cids:
            continue  # leave it as-is if no connections to other communities

        target_cid = neighbor_cids.most_common(1)[0][0]
        for nid in members:
            community_by_node[nid] = target_cid
            communities.setdefault(target_cid, []).append(nid)
        del communities[cid]
        merged_count += 1

    if merged_count:
        print(f"  Merged {merged_count} thin communities")

    # Remove empty communities
    communities = {cid: members for cid, members in communities.items() if members}

    return graph, communities


# ---------------------------------------------------------------------------
# Phase 7: Enhanced Report
# ---------------------------------------------------------------------------

def enhance_report(
    report: str,
    graph: nx.DiGraph,
    crate_deps: dict[str, list[str]],
    communities: dict[int, list[str]],
    labels: dict[int, str],
) -> str:
    """Post-process the generated report to add agent-useful sections."""
    sections: list[str] = []

    # --- Crate Dependency Map ---
    dep_section = "\n## Crate Dependency Map\n\n"
    # Build tier assignments
    all_crates = set()
    for c in crate_deps:
        all_crates.add(c)
        for d in crate_deps[c]:
            all_crates.add(d)
    # Also include crates with no deps
    for nid in graph.nodes:
        if nid.startswith("crate:"):
            all_crates.add(nid.removeprefix("crate:"))

    # Compute tiers: tier 0 = no phantom deps, tier N = max(dep tiers) + 1
    tiers: dict[str, int] = {}

    def get_tier(crate: str, visited: set[str] | None = None) -> int:
        if crate in tiers:
            return tiers[crate]
        if visited is None:
            visited = set()
        if crate in visited:
            return 0
        visited.add(crate)
        deps = crate_deps.get(crate, [])
        if not deps:
            tiers[crate] = 0
            return 0
        t = max(get_tier(d, visited) for d in deps) + 1
        tiers[crate] = t
        return t

    for c in sorted(all_crates):
        get_tier(c)

    max_tier = max(tiers.values()) if tiers else 0
    dep_section += "| Tier | Crates | Dependencies |\n|------|--------|-------------|\n"
    for t in range(max_tier + 1):
        tier_crates = sorted(c for c, tier in tiers.items() if tier == t)
        for c in tier_crates:
            deps = ", ".join(sorted(crate_deps.get(c, []))) or "(none)"
            dep_section += f"| {t} | `{c}` | {deps} |\n"
    sections.append(dep_section)

    # --- Cross-Crate Coupling ---
    coupling_section = "\n## Cross-Crate Coupling (most imported types)\n\n"
    # Count how many distinct source crates import each target entity
    import_counts: Counter[str] = Counter()
    for u, v, data in graph.edges(data=True):
        if data.get("relation") == "uses" and data.get("cross_crate"):
            target_label = graph.nodes[v].get("label", v)
            import_counts[target_label] += 1

    if import_counts:
        coupling_section += "| Entity | Imported by N files |\n|--------|--------------------|\n"
        for label, count in import_counts.most_common(15):
            coupling_section += f"| `{label}` | {count} |\n"
    else:
        coupling_section += "No cross-crate imports detected.\n"
    sections.append(coupling_section)

    # --- Test Coverage Summary ---
    test_section = "\n## Test Coverage Summary\n\n"
    test_nodes = sum(1 for _, d in graph.nodes(data=True) if d.get("is_test"))
    prod_nodes = sum(1 for _, d in graph.nodes(data=True) if not d.get("is_test") and not d.get("is_synthetic"))
    test_section += f"- {prod_nodes} production entities, {test_nodes} test entities\n"
    test_section += f"- Test nodes are tagged `is_test: true` in graph.json but excluded from community detection\n"
    sections.append(test_section)

    # Inject after ## Summary
    injection = "\n".join(sections)
    marker = "## God Nodes"
    if marker in report:
        report = report.replace(marker, injection + "\n" + marker)
    else:
        report += injection

    return report


# ---------------------------------------------------------------------------
# Existing: Relativize, Normalize, Build, Export, Validate
# ---------------------------------------------------------------------------

def relativize_extraction(extraction: dict, root: Path) -> dict:
    def relativize(path_str: str | None) -> str | None:
        if not path_str:
            return path_str
        try:
            return str(Path(path_str).resolve().relative_to(root))
        except Exception:
            return path_str

    normalized = dict(extraction)
    normalized["nodes"] = []
    normalized["edges"] = []

    for node in extraction.get("nodes", []):
        rewritten = dict(node)
        rewritten["source_file"] = relativize(node.get("source_file"))
        normalized["nodes"].append(rewritten)

    for edge in extraction.get("edges", []):
        rewritten = dict(edge)
        rewritten["source_file"] = relativize(edge.get("source_file"))
        normalized["edges"].append(rewritten)

    return normalized


def normalize_extraction(extraction: dict) -> tuple[dict, dict[str, list[str]]]:
    nodes = extraction.get("nodes", [])
    edges = extraction.get("edges", [])

    id_to_nodes: dict[str, list[dict]] = defaultdict(list)
    for node in nodes:
        id_to_nodes[node["id"]].append(node)

    duplicated_ids = {node_id for node_id, items in id_to_nodes.items() if len(items) > 1}
    renamed_nodes: dict[tuple[str, str], str] = {}
    new_nodes: list[dict] = []

    for node in nodes:
        source_file = node.get("source_file")
        old_id = node["id"]
        if old_id in duplicated_ids and source_file:
            new_id = make_scoped_id(source_file, old_id)
            renamed_nodes[(old_id, source_file)] = new_id
        else:
            new_id = old_id
        new_node = dict(node)
        new_node["id"] = new_id
        new_nodes.append(new_node)

    node_ids = {node["id"] for node in new_nodes}

    def resolve(edge_endpoint: str, edge_source_file: str | None, relation: str, role: str) -> str:
        if edge_endpoint not in duplicated_ids:
            return edge_endpoint

        if edge_source_file and (edge_endpoint, edge_source_file) in renamed_nodes:
            return renamed_nodes[(edge_endpoint, edge_source_file)]

        if role == "target" and relation in LOCAL_RELATIONS and edge_source_file:
            scoped = renamed_nodes.get((edge_endpoint, edge_source_file))
            if scoped:
                return scoped

        candidates = [node for node in id_to_nodes[edge_endpoint] if node.get("source_file")]
        if len(candidates) == 1:
            scoped = renamed_nodes.get((edge_endpoint, candidates[0]["source_file"]))
            if scoped:
                return scoped

        return edge_endpoint

    new_edges: list[dict] = []
    unresolved_duplicates: dict[str, list[str]] = defaultdict(list)
    for edge in edges:
        source_file = edge.get("source_file")
        new_edge = dict(edge)
        new_edge["source"] = resolve(edge["source"], source_file, edge.get("relation", ""), "source")
        new_edge["target"] = resolve(edge["target"], source_file, edge.get("relation", ""), "target")
        if new_edge["source"] not in node_ids or new_edge["target"] not in node_ids:
            unresolved_duplicates[edge.get("relation", "unknown")].append(
                f"{edge['source']}->{edge['target']} @ {source_file or 'unknown'}"
            )
            continue
        new_edges.append(new_edge)

    normalized = dict(extraction)
    normalized["nodes"] = new_nodes
    normalized["edges"] = new_edges
    duplicate_map = {
        node_id: [item.get("source_file", "<unknown>") for item in items]
        for node_id, items in id_to_nodes.items()
        if node_id in duplicated_ids
    }
    if unresolved_duplicates:
        normalized["repair_warnings"] = {
            "dropped_edges": unresolved_duplicates,
        }
    return normalized, duplicate_map


def build_directed_graph(extraction: dict) -> nx.DiGraph:
    graph = nx.DiGraph()
    for node in extraction.get("nodes", []):
        graph.add_node(node["id"], **{k: v for k, v in node.items() if k != "id"})
    node_ids = set(graph.nodes)
    for edge in extraction.get("edges", []):
        source = edge["source"]
        target = edge["target"]
        if source not in node_ids or target not in node_ids:
            continue
        attrs = {k: v for k, v in edge.items() if k not in ("source", "target")}
        attrs["_src"] = source
        attrs["_tgt"] = target
        graph.add_edge(source, target, **attrs)
    graph.graph["hyperedges"] = extraction.get("hyperedges", [])
    return graph


def node_community_map(communities: dict[int, list[str]]) -> dict[str, int]:
    result: dict[str, int] = {}
    for community_id, members in communities.items():
        for member in members:
            result[member] = community_id
    return result


def export_graph(graph: nx.DiGraph, communities: dict[int, list[str]], output_path: Path) -> None:
    community_by_node = node_community_map(communities)
    data = json_graph.node_link_data(graph, edges="links")
    for node in data["nodes"]:
        node["community"] = community_by_node.get(node["id"])
    for link in data["links"]:
        if "confidence_score" not in link:
            confidence = link.get("confidence", "EXTRACTED")
            link["confidence_score"] = {"EXTRACTED": 1.0, "INFERRED": 0.7, "AMBIGUOUS": 0.4}.get(confidence, 1.0)
    data["hyperedges"] = graph.graph.get("hyperedges", [])
    output_path.write_text(json.dumps(data, indent=2))


def validate_graph_json(graph_json_path: Path) -> list[str]:
    data = json.loads(graph_json_path.read_text())
    errors: list[str] = []

    if not data.get("directed", False):
        errors.append("graph.json is not directed")

    node_ids = [node["id"] for node in data.get("nodes", [])]
    duplicate_ids = [node_id for node_id, count in Counter(node_ids).items() if count > 1]
    if duplicate_ids:
        errors.append(f"duplicate node ids detected: {duplicate_ids[:10]}")

    node_sources = {node["id"]: node.get("source_file") for node in data.get("nodes", [])}
    for index, link in enumerate(data.get("links", []), start=1):
        source = link.get("source")
        target = link.get("target")
        if source != link.get("_src") or target != link.get("_tgt"):
            errors.append(f"link {index} has mismatched source/_src or target/_tgt")
            break
        if source not in node_sources or target not in node_sources:
            errors.append(f"link {index} references missing node ids")
            break
        relation = link.get("relation", "")
        # Skip cross-crate relation checks — they are expected to cross files
        if relation in CROSS_CRATE_RELATIONS:
            continue
        if relation == "contains" and node_sources[source] != node_sources[target]:
            errors.append(
                f"contains edge crosses files: {source} ({node_sources[source]}) -> {target} ({node_sources[target]})"
            )
            break
        if relation in LOCAL_RELATIONS and "__" not in source and "__" not in target:
            if source == target:
                errors.append(f"self-loop on local relation at link {index}")
                break

    return errors


def write_metadata(
    output_path: Path,
    *,
    root: Path,
    detection: dict,
    duplicate_map: dict[str, list[str]],
    graph: nx.DiGraph,
    communities: dict[int, list[str]],
    crate_deps: dict[str, list[str]],
) -> None:
    test_nodes = sum(1 for _, d in graph.nodes(data=True) if d.get("is_test"))
    cross_crate_edges = sum(1 for _, _, d in graph.edges(data=True) if d.get("cross_crate"))
    payload = {
        "root": str(root),
        "total_files": detection.get("total_files", 0),
        "total_words": detection.get("total_words", 0),
        "code_files": len(detection.get("files", {}).get("code", [])),
        "nodes": graph.number_of_nodes(),
        "edges": graph.number_of_edges(),
        "communities": len(communities),
        "test_nodes": test_nodes,
        "cross_crate_edges": cross_crate_edges,
        "crate_dependencies": crate_deps,
        "scoped_duplicate_ids_fixed": duplicate_map,
    }
    output_path.write_text(json.dumps(payload, indent=2))


# ---------------------------------------------------------------------------
# Pipeline
# ---------------------------------------------------------------------------

def stage_build(root: Path, staging_dir: Path) -> BuildArtifacts:
    print("Step 1: Detecting files...")
    detection = detect(root)
    code_files = [Path(path) for path in detection["files"]["code"]]
    if not code_files:
        raise SystemExit("No code files detected")
    print(f"  Found {len(code_files)} code files")

    print("Step 2: Extracting entities and relationships...")
    extraction = relativize_extraction(extract(code_files), root)
    print(f"  Extracted {len(extraction.get('nodes', []))} nodes, {len(extraction.get('edges', []))} edges")

    print("Step 3: Tagging test nodes...")
    extraction = tag_test_nodes(extraction, root)
    test_count = sum(1 for n in extraction["nodes"] if n.get("is_test"))
    print(f"  Tagged {test_count} test nodes")

    print("Step 4: Normalizing duplicate IDs...")
    normalized, duplicate_map = normalize_extraction(extraction)

    print("Step 5: Injecting cross-crate edges...")
    normalized, crate_deps = inject_cross_crate_edges(normalized, root)

    print("Step 6: Building directed graph...")
    graph = build_directed_graph(normalized)
    print(f"  Graph: {graph.number_of_nodes()} nodes, {graph.number_of_edges()} edges")

    print("Step 7: Adding crate summary nodes...")
    add_crate_summary_nodes(graph, root, crate_deps)
    crate_count = sum(1 for n in graph.nodes if n.startswith("crate:"))
    print(f"  Added {crate_count} crate nodes")

    print("Step 8: Clustering (test-filtered)...")
    clustering_graph = build_clustering_subgraph(graph)
    print(f"  Clustering on {clustering_graph.number_of_nodes()} non-test nodes")
    communities = cluster(clustering_graph)
    communities = assign_excluded_nodes(graph, clustering_graph, communities)
    print(f"  Detected {len(communities)} communities")

    print("Step 9: Pruning noise...")
    graph, communities = prune_noise(graph, communities)
    print(f"  After pruning: {graph.number_of_nodes()} nodes, {len(communities)} communities")

    print("Step 10: Labeling communities...")
    labels = label_communities(graph, communities)

    cohesion = score_all(graph, communities)
    gods = god_nodes(clustering_graph)
    surprises = surprising_connections(graph, communities)
    questions = suggest_questions(graph, communities, labels)

    staging_dir.mkdir(parents=True, exist_ok=True)
    graph_json = staging_dir / "graph.json"
    report_md = staging_dir / "GRAPH_REPORT.md"
    metadata_json = staging_dir / "graphify-metadata.json"

    print("Step 11: Exporting graph and report...")
    export_graph(graph, communities, graph_json)

    report_text = generate(
        graph,
        communities,
        cohesion,
        labels,
        gods,
        surprises,
        detection,
        {
            "input": extraction.get("input_tokens", 0),
            "output": extraction.get("output_tokens", 0),
        },
        root.name,
        suggested_questions=questions,
    )
    report_text = enhance_report(report_text, graph, crate_deps, communities, labels)
    report_md.write_text(report_text)

    write_metadata(
        metadata_json,
        root=root,
        detection=detection,
        duplicate_map=duplicate_map,
        graph=graph,
        communities=communities,
        crate_deps=crate_deps,
    )
    return BuildArtifacts(graph_json=graph_json, report_md=report_md, metadata_json=metadata_json)


def publish(staged: BuildArtifacts, destination: Path) -> None:
    destination.mkdir(parents=True, exist_ok=True)
    shutil.copy2(staged.graph_json, destination / "graph.json")
    shutil.copy2(staged.report_md, destination / "GRAPH_REPORT.md")
    shutil.copy2(staged.metadata_json, destination / "graphify-metadata.json")


def main(argv: Iterable[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description="Rebuild graphify output safely with scoped ids and validation.")
    parser.add_argument("--root", default=".", help="Repository root to scan.")
    parser.add_argument("--output", default="graphify-out", help="Canonical output directory.")
    parser.add_argument(
        "--staging-dir",
        default=None,
        help="Optional staging directory. Defaults to a temporary directory under .graphify-tmp/.",
    )
    parser.add_argument("--keep-staging", action="store_true", help="Keep the staged build directory on success.")
    args = parser.parse_args(list(argv) if argv is not None else None)

    root = Path(args.root).resolve()
    output = (root / args.output).resolve()

    if args.staging_dir:
        staging_dir = Path(args.staging_dir).resolve()
        cleanup = False
    else:
        tmp_root = root / ".graphify-tmp"
        tmp_root.mkdir(exist_ok=True)
        staging_dir = Path(tempfile.mkdtemp(prefix="build-", dir=tmp_root))
        cleanup = not args.keep_staging

    try:
        artifacts = stage_build(root, staging_dir)
        errors = validate_graph_json(artifacts.graph_json)
        if errors:
            print("Graph validation failed:", file=sys.stderr)
            for error in errors:
                print(f"  - {error}", file=sys.stderr)
            return 1
        publish(artifacts, output)
        print(f"Published graph to {output}")
        print(f"Staged build at {staging_dir}")
        return 0
    finally:
        if cleanup and staging_dir.exists():
            shutil.rmtree(staging_dir, ignore_errors=True)


if __name__ == "__main__":
    raise SystemExit(main())
