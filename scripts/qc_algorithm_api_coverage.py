#!/usr/bin/env python3
"""Compare LEAN-facing Python API coverage against local rlean stubs.

The script fetches the upstream QCAlgorithm class reference, combines it with
curated API surfaces used by local strategy examples, compares everything with
the embedded `AlgorithmImports.pyi` stub, and writes JSON plus SVG reports.
"""

from __future__ import annotations

import ast
import html
import json
import re
import sys
import subprocess
import urllib.error
import urllib.request
from dataclasses import dataclass
from pathlib import Path


QCALGORITHM_DOCS_URL = (
    "https://www.lean.io/docs/v2/lean-engine/class-reference/py/"
    "QuantConnect/Algorithm/QCAlgorithm/"
)
QUANTBOOK_DOCS_URL = (
    "https://www.lean.io/docs/v2/lean-engine/class-reference/py/"
    "QuantConnect/Research/QuantBook/"
)
OUTPUT_DIR = "target/api-coverage"
GENERATED_STUB_DIR = "generated-stubs"
GENERATED_STUB_FILE = "AlgorithmImports.pyi"
FETCH_TIMEOUT_SECONDS = 30.0


TRACKED_SURFACES: dict[str, set[str]] = {
    "algorithm_imports": {
        "AccountType",
        "AlphaModel",
        "AverageTrueRange",
        "BollingerBands",
        "BrokerageModelSecurityInitializer",
        "BrokerageName",
        "DataNormalizationMode",
        "EqualWeightingPortfolioConstructionModel",
        "ExecutionModel",
        "ExponentialMovingAverage",
        "FuncSecuritySeeder",
        "Greeks",
        "HyperliquidUniverse",
        "ImmediateExecutionModel",
        "Insight",
        "InsightDirection",
        "MovingAverageConvergenceDivergence",
        "MovingAverageType",
        "NullRiskManagementModel",
        "OptionChain",
        "OptionContract",
        "OptionRight",
        "OrderEvent",
        "OrderStatus",
        "OrderTicket",
        "OrderType",
        "Portfolio",
        "PortfolioBias",
        "PortfolioConstructionModel",
        "PortfolioTarget",
        "QCAlgorithm",
        "RelativeStrengthIndex",
        "Resolution",
        "RiskManagementModel",
        "Security",
        "SecurityChanges",
        "SecurityHolding",
        "SecurityType",
        "SimpleMovingAverage",
        "Slice",
        "Symbol",
        "TimeInForce",
        "TradeBar",
        "UniverseSettings",
    },
    "framework": {
        "AlphaModel",
        "AlphaModel.Update",
        "AlphaModel.OnSecuritiesChanged",
        "PortfolioConstructionModel",
        "PortfolioConstructionModel.CreateTargets",
        "PortfolioConstructionModel.GetTargetInsights",
        "ExecutionModel",
        "ExecutionModel.Execute",
        "RiskManagementModel",
        "RiskManagementModel.ManageRisk",
        "ImmediateExecutionModel",
        "NullRiskManagementModel",
        "EqualWeightingPortfolioConstructionModel",
        "Insight",
        "Insight.Price",
        "InsightDirection",
        "InsightDirection.Up",
        "InsightDirection.Down",
        "InsightDirection.Flat",
        "PortfolioTarget",
        "PortfolioTarget.Percent",
        "PortfolioBias",
        "PortfolioBias.LongShort",
        "PortfolioBias.Long",
        "PortfolioBias.Short",
    },
    "universe": {
        "UniverseSettings",
        "UniverseSettings.resolution",
        "UniverseSettings.Resolution",
        "UniverseSettings.leverage",
        "UniverseSettings.Leverage",
        "SecurityChanges",
        "SecurityChanges.added_securities",
        "SecurityChanges.AddedSecurities",
        "SecurityChanges.removed_securities",
        "SecurityChanges.RemovedSecurities",
        "QCAlgorithm.add_universe",
        "QCAlgorithm.AddUniverse",
        "QCAlgorithm.add_crypto_universe",
        "QCAlgorithm.AddCryptoUniverse",
        "QCAlgorithm.add_hyperliquid_universe",
        "QCAlgorithm.on_securities_changed",
        "HyperliquidUniverse",
        "HyperliquidUniverse.HIP3_XYZ",
        "HyperliquidUniverse.HIP3_TRADING_XYZ",
        "HyperliquidUniverse.hip3",
    },
    "market_data": {
        "Slice",
        "Slice.bars",
        "Slice.Bars",
        "Slice.custom",
        "Slice.get",
        "Slice.get_bar",
        "Slice.GetBar",
        "TradeBar",
        "TradeBar.open",
        "TradeBar.high",
        "TradeBar.low",
        "TradeBar.close",
        "TradeBar.volume",
        "TradeBar.Open",
        "TradeBar.High",
        "TradeBar.Low",
        "TradeBar.Close",
        "TradeBar.Volume",
        "QCAlgorithm.history",
        "QCAlgorithm.History",
        "QCAlgorithm.history_range",
        "QCAlgorithm.AddData",
        "QCAlgorithm.add_data",
    },
    "orders": {
        "OrderTicket",
        "OrderTicket.cancel",
        "OrderTicket.Cancel",
        "OrderTicket.update",
        "OrderTicket.Update",
        "OrderEvent",
        "OrderStatus",
        "OrderType",
        "TimeInForce",
        "QCAlgorithm.market_order",
        "QCAlgorithm.MarketOrder",
        "QCAlgorithm.buy",
        "QCAlgorithm.Buy",
        "QCAlgorithm.sell",
        "QCAlgorithm.Sell",
        "QCAlgorithm.limit_order",
        "QCAlgorithm.LimitOrder",
        "QCAlgorithm.stop_market_order",
        "QCAlgorithm.StopMarketOrder",
        "QCAlgorithm.liquidate",
        "QCAlgorithm.Liquidate",
    },
    "options": {
        "OptionChain",
        "OptionChain.contracts",
        "OptionChain.underlying",
        "OptionContract",
        "OptionContract.strike",
        "OptionContract.expiry",
        "OptionContract.right",
        "OptionContract.bid_price",
        "OptionContract.ask_price",
        "OptionContract.greeks",
        "OptionRight",
        "OptionRight.Call",
        "OptionRight.Put",
        "Greeks",
        "Greeks.delta",
        "Greeks.gamma",
        "Greeks.theta",
        "Greeks.vega",
        "QCAlgorithm.add_option",
        "QCAlgorithm.AddOption",
        "QCAlgorithm.add_option_contract",
        "QCAlgorithm.AddOptionContract",
        "QCAlgorithm.remove_option_contract",
        "QCAlgorithm.RemoveOptionContract",
        "QCAlgorithm.get_option_chain",
        "QCAlgorithm.calculate_implied_volatility",
        "QCAlgorithm.sell_to_open",
        "QCAlgorithm.buy_to_open",
        "QCAlgorithm.buy_to_close",
        "QCAlgorithm.sell_to_close",
    },
    "indicators": {
        "SimpleMovingAverage",
        "ExponentialMovingAverage",
        "RelativeStrengthIndex",
        "MovingAverageConvergenceDivergence",
        "BollingerBands",
        "AverageTrueRange",
        "IndicatorDataPoint",
        "IndicatorResult",
        "MovingAverageType",
        "MovingAverageType.Wilders",
        "QCAlgorithm.SMA",
        "QCAlgorithm.EMA",
        "QCAlgorithm.RSI",
        "QCAlgorithm.STD",
        "QCAlgorithm.MOMP",
        "QCAlgorithm.MACD",
    },
    "portfolio_security": {
        "Security",
        "Security.symbol",
        "Security.Symbol",
        "Security.set_market_price",
        "Security.SetMarketPrice",
        "SecurityHolding",
        "SecurityHolding.symbol",
        "SecurityHolding.Symbol",
        "SecurityHolding.quantity",
        "SecurityHolding.Quantity",
        "SecurityHolding.invested",
        "SecurityHolding.Invested",
        "Portfolio",
        "Portfolio.cash",
        "Portfolio.total_portfolio_value",
        "Portfolio.TotalPortfolioValue",
        "Portfolio.holdings",
        "Portfolio.get_holding",
        "QCAlgorithm.portfolio",
        "QCAlgorithm.Portfolio",
        "QCAlgorithm.securities",
        "QCAlgorithm.Securities",
        "QCAlgorithm.set_brokerage_model",
        "QCAlgorithm.SetBrokerageModel",
        "BrokerageName",
        "AccountType",
    },
    "scheduling": {
        "DateRules",
        "DateRules.every_day",
        "DateRules.EveryDay",
        "TimeRules",
        "TimeRules.at",
        "TimeRules.At",
        "TimeRules.every",
        "TimeRules.Every",
        "ScheduledUniverse",
        "ScheduledUniverse.get_trigger_times",
        "ScheduledUniverse.GetTriggerTimes",
        "QCAlgorithm.date_rules",
        "QCAlgorithm.DateRules",
        "QCAlgorithm.time_rules",
        "QCAlgorithm.TimeRules",
    },
}


@dataclass(frozen=True)
class Coverage:
    expected: set[str]
    local: set[str]

    @property
    def covered(self) -> set[str]:
        return self.expected & self.local

    @property
    def missing(self) -> set[str]:
        return self.expected - self.local

    @property
    def local_only(self) -> set[str]:
        return self.local - self.expected

    @property
    def ratio(self) -> float:
        if not self.expected:
            return 0.0
        return len(self.covered) / len(self.expected)


@dataclass(frozen=True)
class ApiSignature:
    name: str
    positional: tuple[str, ...]
    returns: str | None

    @property
    def arity(self) -> int:
        return len(self.positional)

    def normalized_return(self) -> str | None:
        return normalize_type_name(self.returns) if self.returns else None


@dataclass(frozen=True)
class SignatureMismatch:
    item: str
    expected: str
    local: str
    reason: str


@dataclass(frozen=True)
class ApiAudit:
    docs_signatures: dict[str, list[ApiSignature]]
    local_signatures: dict[str, ApiSignature]
    signature_matches: set[str]
    signature_mismatches: list[SignatureMismatch]
    docs_without_signature: set[str]
    local_only_generated: set[str]


def repo_root() -> Path:
    return Path(__file__).resolve().parents[1]


def normalize_member(name: str) -> str:
    return html.unescape(name).replace("\\_", "_").strip()


def fetch_text(url: str) -> str:
    request = urllib.request.Request(
        url,
        headers={
            "User-Agent": "rlean-api-coverage/1.0",
            "Accept": "text/html,text/markdown,text/plain;q=0.9,*/*;q=0.8",
        },
    )
    with urllib.request.urlopen(request, timeout=FETCH_TIMEOUT_SECONDS) as response:
        charset = response.headers.get_content_charset() or "utf-8"
        return response.read().decode(charset, errors="replace")


def extract_documented_members(text: str) -> set[str]:
    markdown_members = extract_documented_members_from_markdown(text)
    if markdown_members:
        return markdown_members
    heading_members = extract_documented_members_from_headings(text)
    if heading_members:
        return heading_members
    return extract_documented_members_from_html(text)


def extract_documented_members_from_markdown(text: str) -> set[str]:
    members: list[str] = []
    in_qc_toc = False
    toc_indent: int | None = None

    for line in text.splitlines():
        match = re.match(r"^(?P<indent>\s*)\*\s+(?P<name>[A-Za-z_][A-Za-z0-9_\\]*)\s*$", line)
        if not match:
            continue

        indent = len(match.group("indent"))
        name = normalize_member(match.group("name"))

        if name == "QCAlgorithm":
            in_qc_toc = True
            toc_indent = indent
            continue

        if not in_qc_toc:
            continue

        if toc_indent is not None and indent <= toc_indent:
            break

        if is_public_python_member(name):
            members.append(name)

    return set(members)


def extract_documented_members_from_headings(text: str) -> set[str]:
    members: list[str] = []
    for line in text.splitlines():
        match = re.match(r"^#{3,6}\s+(?P<name>[A-Za-z_][A-Za-z0-9_\\]*)\s*$", line)
        if match:
            name = normalize_member(match.group("name"))
            if is_public_python_member(name):
                members.append(name)
    return set(members)


def extract_documented_members_from_html(text: str) -> set[str]:
    candidates = re.findall(r'id=["\'](?:[A-Za-z_][A-Za-z0-9_]*\.)+([A-Za-z_][A-Za-z0-9_]*)["\']', text)
    candidates.extend(
        re.findall(
            r'href=["\']#(?:[A-Za-z_][A-Za-z0-9_]*\.)+([A-Za-z_][A-Za-z0-9_]*)["\']',
            text,
        )
    )
    return {normalize_member(name) for name in candidates if is_public_python_member(name)}


def is_public_python_member(name: str) -> bool:
    return bool(name) and not name.startswith("_")


def extract_documented_signatures(text: str, class_name: str) -> dict[str, list[ApiSignature]]:
    """Best-effort extraction from mkdocstrings code blocks.

    The LEAN Python API pages render overloads as `<code>` blocks like
    `history(symbol: Symbol, periods: int) -> DataFrame`. We keep every overload
    and compare local stubs against the closest arity match.
    """

    signatures: dict[str, list[ApiSignature]] = {}
    for raw_code in re.findall(r"<code[^>]*>(.*?)</code>", text, flags=re.S):
        code = normalize_code_text(raw_code)
        signature = parse_signature_text(code)
        if not signature or not is_public_python_member(signature.name):
            continue
        signatures.setdefault(f"{class_name}.{signature.name}", []).append(signature)
    return signatures


def normalize_code_text(raw_html: str) -> str:
    text = re.sub(r"<[^>]+>", "", raw_html)
    text = html.unescape(text)
    text = re.sub(r"\s+", " ", text).strip()
    return text


def parse_signature_text(text: str) -> ApiSignature | None:
    match = re.match(
        r"^(?P<name>[A-Za-z_][A-Za-z0-9_]*)\s*\((?P<args>.*)\)\s*(?:->\s*(?P<returns>.+))?$",
        text,
    )
    if not match:
        return None
    name = normalize_member(match.group("name"))
    args = parse_signature_args(match.group("args"))
    returns = clean_type_text(match.group("returns")) if match.group("returns") else None
    return ApiSignature(name=name, positional=tuple(args), returns=returns)


def parse_signature_args(args_text: str) -> list[str]:
    args: list[str] = []
    for arg in split_top_level_commas(args_text):
        arg = arg.strip()
        if not arg or arg in {"self", "*", "/"}:
            continue
        arg = arg.lstrip("*")
        name = arg.split(":", 1)[0].split("=", 1)[0].strip()
        if name and name != "self":
            args.append(name)
    return args


def split_top_level_commas(text: str) -> list[str]:
    parts: list[str] = []
    start = 0
    depth = 0
    for idx, ch in enumerate(text):
        if ch in "([{":
            depth += 1
        elif ch in ")]}":
            depth = max(0, depth - 1)
        elif ch == "," and depth == 0:
            parts.append(text[start:idx])
            start = idx + 1
    tail = text[start:]
    if tail.strip():
        parts.append(tail)
    return parts


def clean_type_text(text: str | None) -> str | None:
    if text is None:
        return None
    return re.sub(r"\s+", " ", text).strip().rstrip(".")


def normalize_type_name(type_text: str | None) -> str | None:
    if type_text is None:
        return None
    text = clean_type_text(type_text) or ""
    text = text.replace("NoneType", "None")
    text = re.sub(r"\bOptional\s*\[\s*(.*?)\s*\]", r"\1|None", text)
    text = text.replace("typing.", "")
    text = text.replace("QuantConnect.", "")
    text = text.replace("System.", "")
    text = re.sub(r"\s+", "", text)
    return text


def generate_stubs_with_cargo(root: Path, output_dir: Path) -> Path:
    output_dir.mkdir(parents=True, exist_ok=True)
    command = [
        "cargo",
        "run",
        "-p",
        "rlean",
        "--",
        "stubs",
        "create",
        "--output",
        str(output_dir),
    ]
    subprocess.run(command, cwd=root, check=True)

    stub_path = output_dir / GENERATED_STUB_FILE
    if not stub_path.exists():
        raise FileNotFoundError(f"expected generated stub at {stub_path}")
    return stub_path


@dataclass(frozen=True)
class StubIndex:
    exports: set[str]
    classes: dict[str, set[str]]
    signatures: dict[str, ApiSignature]

    @property
    def all_items(self) -> set[str]:
        items = set(self.exports)
        for class_name, members in self.classes.items():
            items.update(f"{class_name}.{member}" for member in aliases_for(members))
        return items

    @property
    def generated_items(self) -> set[str]:
        items = set(self.exports)
        for class_name, members in self.classes.items():
            items.update(f"{class_name}.{member}" for member in members)
        return items


def build_stub_index(stub_text: str) -> StubIndex:
    module = ast.parse(stub_text)
    exports: set[str] = set()
    classes: dict[str, set[str]] = {}
    signatures: dict[str, ApiSignature] = {}

    for node in module.body:
        if isinstance(node, ast.ClassDef) and is_public_python_member(node.name):
            exports.add(node.name)
            members: set[str] = set()
            for item in node.body:
                member_name = class_member_name(item)
                if member_name and is_public_python_member(member_name):
                    members.add(member_name)
                    signature = local_member_signature(node.name, item, member_name)
                    if signature:
                        signatures[f"{node.name}.{member_name}"] = signature
            classes[node.name] = members

    return StubIndex(exports=exports, classes=classes, signatures=signatures)


def class_member_name(node: ast.stmt) -> str | None:
    if isinstance(node, ast.FunctionDef):
        return node.name
    if isinstance(node, ast.AnnAssign) and isinstance(node.target, ast.Name):
        return node.target.id
    if isinstance(node, ast.Assign) and len(node.targets) == 1 and isinstance(node.targets[0], ast.Name):
        return node.targets[0].id
    return None


def local_member_signature(
    class_name: str,
    node: ast.stmt,
    member_name: str,
) -> ApiSignature | None:
    if isinstance(node, ast.FunctionDef):
        args = [
            arg.arg
            for arg in node.args.args
            if arg.arg != "self" and is_public_python_member(arg.arg)
        ]
        args.extend(arg.arg for arg in node.args.kwonlyargs if is_public_python_member(arg.arg))
        returns = annotation_to_text(node.returns)
        return ApiSignature(name=member_name, positional=tuple(args), returns=returns)
    if isinstance(node, ast.AnnAssign):
        return ApiSignature(
            name=member_name,
            positional=(),
            returns=annotation_to_text(node.annotation),
        )
    return None


def annotation_to_text(node: ast.AST | None) -> str | None:
    if node is None:
        return None
    try:
        return ast.unparse(node)
    except Exception:
        return None


def extract_local_qcalgorithm_members(stub_index: StubIndex) -> set[str]:
    try:
        return stub_index.classes["QCAlgorithm"]
    except KeyError as exc:
        raise ValueError("could not find class QCAlgorithm in local stub content") from exc


def matching_local_items(stub_index: StubIndex, expected: set[str]) -> set[str]:
    universe = stub_index.all_items
    return {item for item in universe if item in expected}


def local_items_for_classes(stub_index: StubIndex, class_names: set[str]) -> set[str]:
    items: set[str] = set()
    for class_name in class_names:
        if class_name in stub_index.exports:
            items.add(class_name)
        for member in aliases_for(stub_index.classes.get(class_name, set())):
            items.add(f"{class_name}.{member}")
    return items


def qcalgorithm_local_items(stub_index: StubIndex) -> set[str]:
    members = extract_local_qcalgorithm_members(stub_index)
    return {f"QCAlgorithm.{name}" for name in aliases_for(members)}


def qcalgorithm_expected_items(docs_members: set[str]) -> set[str]:
    return {f"QCAlgorithm.{name}" for name in docs_members}


def aliases_for(names: set[str]) -> set[str]:
    aliases: set[str] = set()
    for name in names:
        aliases.add(name)
        aliases.add(to_pascal_case(name))
    return aliases


def to_pascal_case(name: str) -> str:
    if "_" not in name:
        return name[:1].upper() + name[1:] if name else name
    return "".join(part[:1].upper() + part[1:] for part in name.split("_") if part)


CANONICAL_MEMBER_CLASSES = {
    "AlphaModel",
    "DateRules",
    "ExecutionModel",
    "Greeks",
    "Insight",
    "OptionChain",
    "OptionContract",
    "OrderTicket",
    "Portfolio",
    "PortfolioConstructionModel",
    "PortfolioTarget",
    "QCAlgorithm",
    "QuantBook",
    "RiskManagementModel",
    "ScheduledUniverse",
    "Security",
    "SecurityChanges",
    "SecurityHolding",
    "Slice",
    "TimeRules",
    "TradeBar",
    "UniverseSettings",
}


def to_snake_case(name: str) -> str:
    if not name or "_" in name or name.isupper():
        return name
    first_pass = re.sub(r"(.)([A-Z][a-z]+)", r"\1_\2", name)
    return re.sub(r"([a-z0-9])([A-Z])", r"\1_\2", first_pass).lower()


def canonical_item(item: str) -> str:
    if ":" in item:
        section, rest = item.split(":", 1)
        return f"{section}:{canonical_item(rest)}"
    if "." not in item:
        return item
    class_name, member_name = item.split(".", 1)
    if class_name not in CANONICAL_MEMBER_CLASSES:
        return item
    return f"{class_name}.{to_snake_case(member_name)}"


def canonical_items(items: set[str]) -> set[str]:
    return {canonical_item(item) for item in items}


def build_section_coverages(
    qcalgorithm_docs_members: set[str],
    quantbook_docs_members: set[str],
    stub_index: StubIndex,
) -> dict[str, Coverage]:
    sections: dict[str, Coverage] = {}

    qca_expected = qcalgorithm_expected_items(qcalgorithm_docs_members)
    sections["qcalgorithm"] = Coverage(
        expected=canonical_items(qca_expected),
        local=canonical_items(qcalgorithm_local_items(stub_index)),
    )

    quantbook_expected = {f"QuantBook.{name}" for name in quantbook_docs_members}
    sections["research_quantbook"] = Coverage(
        expected=canonical_items(quantbook_expected),
        local=canonical_items(
            {f"QuantBook.{name}" for name in aliases_for(stub_index.classes.get("QuantBook", set()))}
        ),
    )

    for section, expected in TRACKED_SURFACES.items():
        class_names = {item for item in expected if "." not in item}
        local = matching_local_items(stub_index, expected)
        local.update(local_items_for_classes(stub_index, class_names) - expected)
        sections[section] = Coverage(expected=canonical_items(expected), local=canonical_items(local))

    return sections


def combine_coverages(sections: dict[str, Coverage]) -> Coverage:
    expected: set[str] = set()
    local: set[str] = set()
    for name, coverage in sections.items():
        expected.update(f"{name}:{item}" for item in coverage.expected)
        local.update(f"{name}:{item}" for item in coverage.local)
    return Coverage(expected=expected, local=local)


def build_api_audit(
    docs_signatures: dict[str, list[ApiSignature]],
    docs_items: set[str],
    stub_index: StubIndex,
) -> ApiAudit:
    local_items = canonical_items(stub_index.generated_items)
    docs_items = canonical_items(docs_items)
    canonical_docs_signatures: dict[str, list[ApiSignature]] = {}
    for item, overloads in docs_signatures.items():
        canonical_docs_signatures.setdefault(canonical_item(item), []).extend(overloads)
    canonical_local_signatures = {
        canonical_item(item): signature for item, signature in stub_index.signatures.items()
    }
    signature_matches: set[str] = set()
    signature_mismatches: list[SignatureMismatch] = []
    docs_without_signature: set[str] = set()

    for item in sorted(docs_items & local_items):
        expected_overloads = canonical_docs_signatures.get(item)
        local_signature = canonical_local_signatures.get(item)
        if not expected_overloads:
            docs_without_signature.add(item)
            continue
        if not local_signature:
            signature_mismatches.append(
                SignatureMismatch(
                    item=item,
                    expected=" | ".join(format_signature(sig) for sig in expected_overloads),
                    local="<no local signature>",
                    reason="local member has no parseable signature",
                )
            )
            continue
        mismatch = compare_signature(item, expected_overloads, local_signature)
        if mismatch is None:
            signature_matches.add(item)
        else:
            signature_mismatches.append(mismatch)

    return ApiAudit(
        docs_signatures=canonical_docs_signatures,
        local_signatures=canonical_local_signatures,
        signature_matches=signature_matches,
        signature_mismatches=signature_mismatches,
        docs_without_signature=docs_without_signature,
        local_only_generated=local_items - docs_items,
    )


def compare_signature(
    item: str,
    expected_overloads: list[ApiSignature],
    local_signature: ApiSignature,
) -> SignatureMismatch | None:
    arity_matches = [sig for sig in expected_overloads if sig.arity == local_signature.arity]
    candidates = arity_matches or expected_overloads
    local_return = local_signature.normalized_return()
    for expected in candidates:
        expected_return = expected.normalized_return()
        if expected.arity != local_signature.arity:
            continue
        if not expected_return or not local_return or expected_return == local_return:
            return None
    reason = "return type mismatch" if arity_matches else "input arity mismatch"
    return SignatureMismatch(
        item=item,
        expected=" | ".join(format_signature(sig) for sig in expected_overloads),
        local=format_signature(local_signature),
        reason=reason,
    )


def format_signature(signature: ApiSignature) -> str:
    args = ", ".join(signature.positional)
    returns = f" -> {signature.returns}" if signature.returns else ""
    return f"{signature.name}({args}){returns}"


def coverage_to_json(coverage: Coverage) -> dict[str, object]:
    return {
        "expected_count": len(coverage.expected),
        "local_count": len(coverage.local),
        "covered_count": len(coverage.covered),
        "missing_count": len(coverage.missing),
        "local_only_count": len(coverage.local_only),
        "coverage_percent": round(coverage.ratio * 100.0, 2),
        "covered": sorted(coverage.covered),
        "missing": sorted(coverage.missing),
        "local_only": sorted(coverage.local_only),
    }


def api_audit_to_json(audit: ApiAudit) -> dict[str, object]:
    return {
        "signature_match_count": len(audit.signature_matches),
        "signature_mismatch_count": len(audit.signature_mismatches),
        "docs_without_signature_count": len(audit.docs_without_signature),
        "local_only_generated_count": len(audit.local_only_generated),
        "signature_matches": sorted(audit.signature_matches),
        "signature_mismatches": [
            {
                "item": mismatch.item,
                "reason": mismatch.reason,
                "expected": mismatch.expected,
                "local": mismatch.local,
            }
            for mismatch in audit.signature_mismatches
        ],
        "docs_without_signature": sorted(audit.docs_without_signature),
        "local_only_generated": sorted(audit.local_only_generated),
    }


def write_json_report(
    path: Path,
    sections: dict[str, Coverage],
    overall: Coverage,
    stub_source: Path,
    api_audit: ApiAudit,
) -> None:
    report = {
        "docs_urls": {
            "qcalgorithm": QCALGORITHM_DOCS_URL,
            "research_quantbook": QUANTBOOK_DOCS_URL,
        },
        "stub_source": str(stub_source),
        "overall": coverage_to_json(overall),
        "sections": {name: coverage_to_json(coverage) for name, coverage in sections.items()},
        "api_audit": api_audit_to_json(api_audit),
    }
    path.write_text(json.dumps(report, indent=2, sort_keys=True) + "\n", encoding="utf-8")


def write_svg_chart(path: Path, sections: dict[str, Coverage], overall: Coverage) -> None:
    width = 1080
    row_height = 42
    top = 132
    margin = 56
    label_width = 220
    chart_width = width - margin * 2 - label_width - 160
    height = top + row_height * (len(sections) + 1) + 72

    rows = [("overall", overall), *sections.items()]
    svg_rows: list[str] = []
    for idx, (name, coverage) in enumerate(rows):
        y = top + idx * row_height
        covered_width = int(chart_width * coverage.ratio) if coverage.expected else 0
        missing_width = chart_width - covered_width if coverage.expected else 0
        label = name.replace("_", " ").title()
        svg_rows.append(
            f'  <text x="{margin}" y="{y + 24}" class="label">{escape_xml(label)}</text>\n'
            f'  <rect x="{margin + label_width}" y="{y}" width="{chart_width}" height="24" rx="6" fill="#f2f4f7"/>\n'
            f'  <rect x="{margin + label_width}" y="{y}" width="{covered_width}" height="24" rx="6" fill="#12b76a"/>\n'
            f'  <rect x="{margin + label_width + covered_width}" y="{y}" width="{missing_width}" height="24" fill="#f04438"/>\n'
            f'  <text x="{margin + label_width + chart_width + 18}" y="{y + 18}" class="small">'
            f'{coverage.ratio * 100.0:.1f}% ({len(coverage.covered)}/{len(coverage.expected)})</text>'
        )

    svg = f"""<svg xmlns="http://www.w3.org/2000/svg" width="{width}" height="{height}" viewBox="0 0 {width} {height}">
  <style>
    .title {{ font: 700 26px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; fill: #172033; }}
    .subtitle {{ font: 15px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; fill: #526070; }}
    .label {{ font: 14px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; fill: #344054; }}
    .value {{ font: 700 18px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; fill: #172033; }}
    .small {{ font: 12px -apple-system, BlinkMacSystemFont, "Segoe UI", sans-serif; fill: #667085; }}
  </style>
  <rect width="100%" height="100%" fill="#ffffff"/>
  <text x="{margin}" y="52" class="title">rlean Python API Coverage</text>
  <text x="{margin}" y="82" class="subtitle">QCAlgorithm docs plus strategy-relevant framework, universe, data, orders, options, indicators, and portfolio surfaces.</text>
  <text x="{margin}" y="112" class="value">{overall.ratio * 100.0:.1f}% overall</text>
{chr(10).join(svg_rows)}
  <circle cx="{margin}" cy="{height - 30}" r="6" fill="#12b76a"/>
  <text x="{margin + 16}" y="{height - 26}" class="small">Covered</text>
  <circle cx="{margin + 100}" cy="{height - 30}" r="6" fill="#f04438"/>
  <text x="{margin + 116}" y="{height - 26}" class="small">Missing</text>
</svg>
"""
    path.write_text(svg, encoding="utf-8")


def escape_xml(text: str) -> str:
    return (
        text.replace("&", "&amp;")
        .replace("<", "&lt;")
        .replace(">", "&gt;")
        .replace('"', "&quot;")
    )


def main() -> int:
    qcalgorithm_docs_text = fetch_text(QCALGORITHM_DOCS_URL)
    qcalgorithm_docs_members = extract_documented_members(qcalgorithm_docs_text)
    if not qcalgorithm_docs_members:
        print("error: no documented QCAlgorithm members were found", file=sys.stderr)
        return 2

    quantbook_docs_text = fetch_text(QUANTBOOK_DOCS_URL)
    quantbook_docs_members = extract_documented_members(quantbook_docs_text)
    if not quantbook_docs_members:
        print("error: no documented QuantBook members were found", file=sys.stderr)
        return 2

    root = repo_root()
    output_dir = root / OUTPUT_DIR
    stub_source = generate_stubs_with_cargo(root, output_dir / GENERATED_STUB_DIR)
    stub_index = build_stub_index(stub_source.read_text(encoding="utf-8"))
    sections = build_section_coverages(qcalgorithm_docs_members, quantbook_docs_members, stub_index)
    overall = combine_coverages(sections)
    docs_signatures = {}
    docs_signatures.update(extract_documented_signatures(qcalgorithm_docs_text, "QCAlgorithm"))
    docs_signatures.update(extract_documented_signatures(quantbook_docs_text, "QuantBook"))
    docs_items = qcalgorithm_expected_items(qcalgorithm_docs_members)
    docs_items.update({f"QuantBook.{name}" for name in quantbook_docs_members})
    for expected in TRACKED_SURFACES.values():
        docs_items.update(expected)
    api_audit = build_api_audit(docs_signatures, docs_items, stub_index)

    output_dir.mkdir(parents=True, exist_ok=True)
    json_path = output_dir / "qc_algorithm_api_coverage.json"
    svg_path = output_dir / "qc_algorithm_api_coverage.svg"
    write_json_report(json_path, sections, overall, stub_source, api_audit)
    write_svg_chart(svg_path, sections, overall)

    print(f"Python API coverage: {overall.ratio * 100.0:.1f}%")
    for name, coverage in sections.items():
        print(
            f"  {name:20} {coverage.ratio * 100.0:5.1f}% "
            f"({len(coverage.covered)}/{len(coverage.expected)})"
        )
    print(
        "  signatures           "
        f"{len(api_audit.signature_matches)} match, "
        f"{len(api_audit.signature_mismatches)} mismatch, "
        f"{len(api_audit.docs_without_signature)} without docs signature"
    )
    print(f"  generated local-only {len(api_audit.local_only_generated)}")
    print(f"  report: {json_path}")
    print(f"  chart:  {svg_path}")
    return 0


if __name__ == "__main__":
    try:
        raise SystemExit(main())
    except urllib.error.URLError as exc:
        print(f"error fetching docs: {exc}", file=sys.stderr)
        raise SystemExit(2)
