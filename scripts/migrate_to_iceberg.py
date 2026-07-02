#!/usr/bin/env python3
"""One-time rlean Parquet-to-Iceberg migration.

This script is intentionally destructive: after source Parquet files are
successfully appended to Iceberg, those source files are deleted immediately.
Restart safety comes from the remaining files on disk.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import shutil
import sqlite3
import datetime as dt
from dataclasses import dataclass
from pathlib import Path
from typing import Callable, Iterable

import pyarrow as pa
import pyarrow.compute as pc
import pyarrow.parquet as pq
from pyiceberg.catalog.sql import SqlCatalog
from pyiceberg.exceptions import NoSuchNamespaceError, NoSuchTableError
from pyiceberg.partitioning import PartitionField, PartitionSpec
from pyiceberg.schema import Schema
from pyiceberg.transforms import IdentityTransform
from pyiceberg.types import (
    BooleanType,
    DateType,
    DoubleType,
    IntegerType,
    LongType,
    NestedField,
    StringType,
)

NAMESPACE = "lean"
NS_PER_DAY = 86_400_000_000_000


@dataclass(frozen=True)
class TargetTable:
    name: str
    schema: Schema
    arrow_schema: pa.Schema
    partition_fields: tuple[str, ...]


@dataclass(frozen=True)
class MigrationItem:
    path: Path
    table: str
    transform: Callable[[pa.Table, Path], pa.Table]


@dataclass
class MigrationBatch:
    table: str
    items: list[MigrationItem]

    @property
    def row_count(self) -> int:
        return len(self.items)


def main() -> None:
    args = parse_args()
    source_root = args.source_root.resolve()
    data_root = args.data_root.resolve()
    warehouse = data_root / "iceberg"
    catalog_db = warehouse / "catalog.db"

    if not source_root.exists():
        raise SystemExit(f"source root does not exist: {source_root}")

    warehouse.mkdir(parents=True, exist_ok=True)
    catalog = load_catalog(catalog_db, warehouse)
    ensure_tables(catalog)

    include_top_level = set(args.include_top_level or [])
    total = 0
    if args.dry_run:
        for item in iter_items(source_root, include_top_level):
            total += 1
            migrate_item(catalog, item, args.dry_run)
            prune_empty_parents(item.path.parent, source_root, args.dry_run)
    else:
        for batch in iter_batches(
            iter_items(source_root, include_top_level),
            max_files=args.batch_files,
            max_rows=args.batch_rows,
        ):
            total += batch.row_count
            migrate_batch(catalog, batch)
            for item in batch.items:
                prune_empty_parents(item.path.parent, source_root, args.dry_run)

    prune_top_level_dirs(source_root, args.dry_run)
    print(f"processed {total} source parquet files", flush=True)


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--source-root",
        default="/Volumes/data_cache/lean/data",
        type=Path,
        help="Existing path-based rlean data directory.",
    )
    parser.add_argument(
        "--data-root",
        default="/Volumes/data_cache/lean",
        type=Path,
        help="rlean data root. The Iceberg catalog/warehouse is created under data-root/iceberg.",
    )
    parser.add_argument(
        "--dry-run",
        action="store_true",
        help="Print planned files without appending or deleting.",
    )
    parser.add_argument(
        "--include-top-level",
        action="append",
        default=None,
        help="Only migrate selected top-level source directories. Repeat for multiple values.",
    )
    parser.add_argument(
        "--batch-files",
        type=int,
        default=500,
        help="Maximum source files per Iceberg append commit.",
    )
    parser.add_argument(
        "--batch-rows",
        type=int,
        default=500_000,
        help="Approximate maximum rows per Iceberg append commit.",
    )
    return parser.parse_args()


def load_catalog(catalog_db: Path, warehouse: Path) -> SqlCatalog:
    initialize_sqlite_catalog(catalog_db)
    return SqlCatalog(
        "rlean",
        **{
            "uri": f"sqlite:///{catalog_db}",
            "warehouse": warehouse.as_uri(),
        },
    )


def initialize_sqlite_catalog(catalog_db: Path) -> None:
    catalog_db.parent.mkdir(parents=True, exist_ok=True)
    with sqlite3.connect(catalog_db) as conn:
        conn.executescript(
            """
            CREATE TABLE IF NOT EXISTS iceberg_tables (
                catalog_name TEXT NOT NULL,
                table_namespace TEXT NOT NULL,
                table_name TEXT NOT NULL,
                metadata_location TEXT,
                previous_metadata_location TEXT,
                PRIMARY KEY (catalog_name, table_namespace, table_name)
            );
            CREATE TABLE IF NOT EXISTS iceberg_namespace_properties (
                catalog_name TEXT NOT NULL,
                namespace TEXT NOT NULL,
                property_key TEXT NOT NULL,
                property_value TEXT,
                PRIMARY KEY (catalog_name, namespace, property_key)
            );
            """
        )


def ensure_tables(catalog: SqlCatalog) -> None:
    try:
        catalog.create_namespace(NAMESPACE)
    except Exception:
        try:
            catalog.load_namespace_properties(NAMESPACE)
        except NoSuchNamespaceError:
            raise

    for target in targets().values():
        ident = f"{NAMESPACE}.{target.name}"
        try:
            catalog.load_table(ident)
            continue
        except NoSuchTableError:
            pass

        location = f"{catalog.properties['warehouse'].rstrip('/')}/{NAMESPACE}/{target.name}"
        catalog.create_table(
            ident,
            schema=target.schema,
            partition_spec=partition_spec(target),
            location=location,
        )


def partition_spec(target: TargetTable) -> PartitionSpec:
    source_ids = {field.name: field.field_id for field in target.schema.fields}
    next_id = 1000
    fields: list[PartitionField] = []
    for name in target.partition_fields:
        fields.append(
            PartitionField(
                source_id=source_ids[name],
                field_id=next_id,
                transform=IdentityTransform(),
                name=name,
            )
        )
        next_id += 1
    return PartitionSpec(*fields)


def targets() -> dict[str, TargetTable]:
    return {
        "market_trade_bars": TargetTable(
            "market_trade_bars",
            schema_from_arrow(market_schema(trade_bar_arrow_schema())),
            market_schema(trade_bar_arrow_schema()),
            ("security_type", "market", "resolution", "day"),
        ),
        "market_quote_bars": TargetTable(
            "market_quote_bars",
            schema_from_arrow(market_schema(quote_bar_arrow_schema())),
            market_schema(quote_bar_arrow_schema()),
            ("security_type", "market", "resolution", "day"),
        ),
        "market_ticks": TargetTable(
            "market_ticks",
            schema_from_arrow(market_schema(tick_arrow_schema())),
            market_schema(tick_arrow_schema()),
            ("security_type", "market", "resolution", "day"),
        ),
        "option_eod_bars": TargetTable(
            "option_eod_bars",
            schema_from_arrow(option_schema(option_eod_arrow_schema())),
            option_schema(option_eod_arrow_schema()),
            ("underlying", "day"),
        ),
        "option_universe": TargetTable(
            "option_universe",
            schema_from_arrow(option_schema(option_universe_arrow_schema())),
            option_schema(option_universe_arrow_schema()),
            ("underlying", "day"),
        ),
        "margin_interest": TargetTable(
            "margin_interest",
            schema_from_arrow(market_schema(margin_interest_arrow_schema())),
            market_schema(margin_interest_arrow_schema()),
            ("security_type", "market", "day"),
        ),
        "perpetual_context": TargetTable(
            "perpetual_context",
            schema_from_arrow(market_schema(perpetual_context_arrow_schema())),
            market_schema(perpetual_context_arrow_schema()),
            ("security_type", "market", "day"),
        ),
        "custom_points": TargetTable(
            "custom_points",
            schema_from_arrow(custom_points_arrow_schema()),
            custom_points_arrow_schema(),
            ("source_type", "ticker", "day"),
        ),
        "factor_files": TargetTable(
            "factor_files",
            schema_from_arrow(factor_file_arrow_schema()),
            factor_file_arrow_schema(),
            ("market", "ticker"),
        ),
        "map_files": TargetTable(
            "map_files",
            schema_from_arrow(map_file_arrow_schema()),
            map_file_arrow_schema(),
            ("market", "permtick"),
        ),
    }


def iter_items(source_root: Path, include_top_level: set[str]) -> Iterable[MigrationItem]:
    roots = (
        [source_root / name for name in sorted(include_top_level)]
        if include_top_level
        else [source_root]
    )
    for scan_root in roots:
        if not scan_root.exists():
            continue
        for path in sorted(scan_root.rglob("*.parquet")):
            item = classify_path(source_root, path)
            if item is None:
                print(f"skip unrecognized parquet path: {path}")
                continue
            yield item


def classify_path(root: Path, path: Path) -> MigrationItem | None:
    rel = path.relative_to(root)
    parts = rel.parts
    if len(parts) >= 5 and parts[0] == "option" and parts[2] == "daily":
        underlying = infer_underlying_from_option_path(path)
        if "universe" in {part.lower() for part in parts} or path.stem == "universe":
            return MigrationItem(path, "option_universe", lambda table, _: add_option_partitions(table, underlying))
        return MigrationItem(path, "option_eod_bars", lambda table, _: add_option_partitions(table, underlying))

    if len(parts) >= 3 and parts[-3] == "factor_files":
        market, ticker = parts[-4], path.stem
        return MigrationItem(path, "factor_files", lambda table, _: add_static_partitions(table, market=market, ticker=ticker.lower()))

    if len(parts) >= 3 and parts[-3] == "map_files":
        market, ticker = parts[-4], path.stem
        return MigrationItem(path, "map_files", lambda table, _: add_static_partitions(table, market=market, ticker=ticker.lower()))

    if parts[0] in {"custom", "alternative"}:
        source_type, ticker = infer_custom_identity(parts, path)
        return MigrationItem(
            path,
            "custom_points",
            lambda table, source_path: add_custom_partitions(table, source_type, ticker, source_path),
        )

    # When scanning directly under `data/alternative/{provider}/`, rel paths omit the
    # `alternative/` prefix (e.g. tradealert/sweeps/_ALL/5min/...).
    if len(parts) >= 3 and root.name == "alternative":
        source_type, ticker = parts[0], parts[1]
        return MigrationItem(
            path,
            "custom_points",
            lambda table, source_path: add_custom_partitions(table, source_type, ticker, source_path),
        )

    if len(parts) >= 6:
        security_type, market, resolution, data_kind = parts[0], parts[1], parts[2], parts[3]
        if data_kind_to_tick_type(data_kind) is not None:
            table = {
                "trade": "market_trade_bars",
                "quote": "market_quote_bars",
                "tick": "market_ticks",
            }.get(data_kind)
            if table:
                return MigrationItem(
                    path,
                    table,
                    lambda table_data, _: add_market_partitions(
                        table_data,
                        security_type=security_type,
                        market=market,
                        resolution=resolution,
                    ),
                )

    if len(parts) >= 5 and parts[-3] in {"margin_interest", "funding"}:
        security_type, market = parts[0], parts[1]
        return MigrationItem(
            path,
            "margin_interest",
            lambda table, _: add_market_partitions(
                table,
                security_type=security_type,
                market=market,
                resolution="",
            ),
        )

    if len(parts) >= 5 and parts[-3] in {"perpetual_context", "context"}:
        security_type, market = parts[0], parts[1]
        return MigrationItem(
            path,
            "perpetual_context",
            lambda table, _: add_market_partitions(
                table,
                security_type=security_type,
                market=market,
                resolution="",
            ),
        )

    return None


def migrate_item(catalog: SqlCatalog, item: MigrationItem, dry_run: bool) -> None:
    print(f"{item.path} -> {NAMESPACE}.{item.table}", flush=True)
    if dry_run:
        return

    source_table = pq.read_table(item.path)
    iceberg_table = catalog.load_table(f"{NAMESPACE}.{item.table}")
    arrow_table = align_arrow_table(item.transform(source_table, item.path), targets()[item.table].arrow_schema)
    iceberg_table.append(arrow_table)
    item.path.unlink()


def iter_batches(
    items: Iterable[MigrationItem],
    *,
    max_files: int,
    max_rows: int,
) -> Iterable[MigrationBatch]:
    if max_files <= 0:
        raise ValueError("--batch-files must be greater than zero")
    if max_rows <= 0:
        raise ValueError("--batch-rows must be greater than zero")

    batch: MigrationBatch | None = None
    batch_rows = 0
    for item in items:
        item_rows = parquet_row_count(item.path)
        if (
            batch is not None
            and (
                batch.table != item.table
                or len(batch.items) >= max_files
                or (batch_rows > 0 and batch_rows + item_rows > max_rows)
            )
        ):
            yield batch
            batch = None
            batch_rows = 0

        if batch is None:
            batch = MigrationBatch(table=item.table, items=[])

        batch.items.append(item)
        batch_rows += item_rows

    if batch is not None and batch.items:
        yield batch


def parquet_row_count(path: Path) -> int:
    return pq.read_metadata(path).num_rows


def migrate_batch(catalog: SqlCatalog, batch: MigrationBatch) -> None:
    first = batch.items[0].path
    last = batch.items[-1].path
    print(
        f"{len(batch.items)} files {first} .. {last} -> {NAMESPACE}.{batch.table}",
        flush=True,
    )

    arrow_tables = []
    target_schema = targets()[batch.table].arrow_schema
    for item in batch.items:
        source_table = pq.read_table(item.path)
        arrow_tables.append(align_arrow_table(item.transform(source_table, item.path), target_schema))

    iceberg_table = catalog.load_table(f"{NAMESPACE}.{batch.table}")
    iceberg_table.append(pa.concat_tables(arrow_tables, promote_options="default"))

    for item in batch.items:
        item.path.unlink()


def align_arrow_table(table: pa.Table, schema: pa.Schema) -> pa.Table:
    columns = []
    for field in schema:
        if field.name not in table.column_names:
            raise ValueError(f"required column {field.name!r} missing from source table")
        column = table[field.name]
        if field.name == "symbol_sid" and pa.types.is_uint64(column.type):
            column = signed_i64_sid_column(column)
        columns.append(column)
    return pa.table(columns, schema=schema)


def signed_i64_sid_column(column: pa.ChunkedArray) -> pa.Array:
    values = []
    for value in column.to_pylist():
        if value is None:
            values.append(None)
            continue
        value = int(value)
        if value > 2**63 - 1:
            value -= 2**64
        values.append(value)
    return pa.array(values, type=pa.int64())


def add_market_partitions(
    table: pa.Table,
    *,
    security_type: str,
    market: str,
    resolution: str,
) -> pa.Table:
    count = table.num_rows
    day_source = "end_time_ns" if "end_time_ns" in table.column_names else "time_ns"
    day = day_from_ns(table[day_source])
    return table.append_column("security_type", string_array(security_type.lower(), count)).append_column(
        "market", string_array(market.lower(), count)
    ).append_column("resolution", string_array(resolution, count)).append_column(
        "day", day
    )


def add_option_partitions(table: pa.Table, underlying: str) -> pa.Table:
    if "underlying" not in table.column_names:
        table = table.append_column("underlying", string_array(underlying.lower(), table.num_rows))
    return table.append_column("day", day_from_ns(table["date_ns"]))


def add_custom_partitions(table: pa.Table, source_type: str, ticker: str, path: Path) -> pa.Table:
    table = normalize_custom_table(table, source_type, ticker, path)
    return table.append_column("source_type", string_array(source_type.lower(), table.num_rows)).append_column(
        "ticker", string_array(ticker.lower(), table.num_rows)
    ).append_column(
        "day", day_from_ns(table["date_ns"])
    )


def normalize_custom_table(table: pa.Table, source_type: str, ticker: str, path: Path) -> pa.Table:
    if {"date_ns", "value", "fields_json"}.issubset(set(table.column_names)):
        return table

    date_ns = custom_date_ns(table, path)
    value_name = custom_value_column(table, source_type, ticker)
    fields = table.to_pylist()
    fields_json = [json.dumps({k: v for k, v in row.items() if v is not None}, default=str) for row in fields]
    values = table[value_name].to_pylist() if value_name else [0.0] * table.num_rows
    values = [float(v) if v is not None else 0.0 for v in values]
    return pa.table(
        {
            "date_ns": pa.array(date_ns, type=pa.int64()),
            "value": pa.array(values, type=pa.float64()),
            "fields_json": pa.array(fields_json, type=pa.string()),
        }
    )


def custom_date_ns(table: pa.Table, path: Path) -> list[int]:
    for name in ("date_ns", "time_ns", "end_time_ns"):
        if name in table.column_names:
            values = table[name].to_pylist()
            return [int(v) if v is not None else 0 for v in values]
    for name in ("release_date", "date", "time", "end_time"):
        if name in table.column_names:
            return [parse_date_ns(v) for v in table[name].to_pylist()]
    if inferred := custom_date_ns_from_path(path):
        return [inferred] * table.num_rows
    raise ValueError(f"custom parquet table has no date/time column and path date could not be inferred: {path}")


def custom_date_ns_from_path(path: Path) -> int | None:
    parts = path.parts
    for idx in range(len(parts) - 2):
        year, month, day = parts[idx : idx + 3]
        if not (year.isdigit() and month.isdigit() and day.isdigit()):
            continue
        if len(year) != 4:
            continue
        try:
            parsed_date = dt.date(int(year), int(month), int(day))
        except ValueError:
            continue
        hour = minute = 0
        stem = path.stem
        if re.fullmatch(r"\d{4}", stem):
            hour = int(stem[:2])
            minute = int(stem[2:])
        elif re.fullmatch(r"\d{6}", stem):
            hour = int(stem[:2])
            minute = int(stem[2:4])
        elif path_has_intraday_frequency(parts):
            hour = 16
            minute = 0
        timestamp = dt.datetime(
            parsed_date.year,
            parsed_date.month,
            parsed_date.day,
            hour,
            minute,
            tzinfo=dt.timezone.utc,
        )
        return int(timestamp.timestamp() * 1_000_000_000)
    return None


def path_has_intraday_frequency(parts: tuple[str, ...]) -> bool:
    return any(re.fullmatch(r"\d+(mi|min|m|sec|s|hour|h)", part.lower()) for part in parts)


def parse_date_ns(value) -> int:
    if value is None:
        return 0
    if isinstance(value, dt.datetime):
        if value.tzinfo is None:
            value = value.replace(tzinfo=dt.timezone.utc)
        return int(value.timestamp() * 1_000_000_000)
    if isinstance(value, dt.date):
        return int(dt.datetime(value.year, value.month, value.day, tzinfo=dt.timezone.utc).timestamp() * 1_000_000_000)
    text = str(value)
    parsed = dt.datetime.fromisoformat(text.replace("Z", "+00:00"))
    if parsed.tzinfo is None:
        parsed = parsed.replace(tzinfo=dt.timezone.utc)
    return int(parsed.timestamp() * 1_000_000_000)


def custom_value_column(table: pa.Table, source_type: str, ticker: str) -> str | None:
    preferred = custom_value_columns(source_type, ticker)
    for name in preferred:
        if name in table.column_names and is_numeric_type(table.schema.field(name).type):
            return name
    for field in table.schema:
        if is_numeric_type(field.type):
            return field.name
    return None


def custom_value_columns(source_type: str, ticker: str) -> tuple[str, ...]:
    source = source_type.lower()
    data_ticker = ticker.lower()
    if source == "tradealert" and data_ticker == "sweeps":
        return ("norm_edge", "size", "value", "price")
    if source == "tradealert" and data_ticker in {"snapshot", "most_active"}:
        return ("option_volume", "atm_ivol", "close", "value", "price")
    if source == "unusual_whales" and data_ticker == "flow_alerts":
        return ("total_premium", "premium", "size", "volume", "value", "price")
    return (
        "value",
        "close",
        "gdp_nowcast",
        "vix_close",
        "price",
        data_ticker,
        ticker.upper(),
        source,
    )


def is_numeric_type(dtype: pa.DataType) -> bool:
    return pa.types.is_floating(dtype) or pa.types.is_integer(dtype)


def add_static_partitions(table: pa.Table, *, market: str, ticker: str) -> pa.Table:
    return table.append_column("market", string_array(market.lower(), table.num_rows)).append_column(
        "permtick", string_array(ticker.lower(), table.num_rows)
    )


def day_from_ns(ns_column: pa.ChunkedArray) -> pa.Array:
    days = pc.divide_checked(ns_column, pa.scalar(NS_PER_DAY, pa.int64())).combine_chunks()
    return pa.array(days.to_pylist(), type=pa.date32())


def string_array(value: str, count: int) -> pa.Array:
    return pa.array([value] * count, type=pa.string())


def infer_underlying_from_option_path(path: Path) -> str:
    for part in reversed(path.parts):
        match = re.match(r"([a-zA-Z]+)", part)
        if match and part.lower() not in {"data", "daily", "option", "usa", "universe", "trade", "quote", "tick"}:
            return match.group(1).lower()
    raise ValueError(f"cannot infer option underlying from {path}")


def infer_custom_identity(parts: tuple[str, ...], path: Path) -> tuple[str, str]:
    if len(parts) >= 3:
        return parts[1], canonical_custom_ticker(parts[1], parts[2])
    if len(parts) == 2:
        return parts[0], canonical_custom_ticker(parts[0], path.stem)
    raise ValueError(f"cannot infer custom source/ticker from {path}")


def canonical_custom_ticker(source_type: str, ticker: str) -> str:
    if source_type.lower() == "tradealert" and ticker.lower() == "underlying_fields_eod":
        return "snapshot"
    return ticker


def data_kind_to_tick_type(data_kind: str) -> str | None:
    return {
        "trade": "trade",
        "quote": "quote",
        "tick": "trade",
    }.get(data_kind)


def prune_empty_parents(start: Path, stop: Path, dry_run: bool) -> None:
    current = start
    while current != stop and stop in current.parents:
        if not current.exists():
            current = current.parent
            continue
        try:
            next(current.iterdir())
            return
        except StopIteration:
            if dry_run:
                print(f"would remove empty directory {current}")
            else:
                current.rmdir()
            current = current.parent


def prune_top_level_dirs(source_root: Path, dry_run: bool) -> None:
    for path in source_root.iterdir():
        if not path.is_dir():
            continue
        try:
            next(path.rglob("*"))
        except StopIteration:
            if dry_run:
                print(f"would remove empty directory tree {path}")
            else:
                shutil.rmtree(path)


def schema_from_arrow(arrow_schema: pa.Schema) -> Schema:
    fields = []
    for idx, field in enumerate(arrow_schema, start=1):
        fields.append(NestedField(idx, field.name, iceberg_type(field.type), required=not field.nullable))
    return Schema(*fields)


def iceberg_type(dtype: pa.DataType):
    if pa.types.is_boolean(dtype):
        return BooleanType()
    if pa.types.is_int8(dtype) or pa.types.is_int16(dtype) or pa.types.is_int32(dtype) or pa.types.is_uint8(dtype):
        return IntegerType()
    if pa.types.is_int64(dtype) or pa.types.is_uint64(dtype):
        return LongType()
    if pa.types.is_float64(dtype):
        return DoubleType()
    if pa.types.is_string(dtype) or pa.types.is_large_string(dtype):
        return StringType()
    if pa.types.is_date32(dtype):
        return DateType()
    raise TypeError(f"unsupported Arrow type {dtype}")


def market_schema(base: pa.Schema) -> pa.Schema:
    return base.append(pa.field("security_type", pa.string(), nullable=False)).append(
        pa.field("market", pa.string(), nullable=False)
    ).append(
        pa.field("resolution", pa.string(), nullable=False)
    ).append(
        pa.field("day", pa.date32(), nullable=False)
    )


def option_schema(base: pa.Schema) -> pa.Schema:
    return base.append(pa.field("day", pa.date32(), nullable=False))


def trade_bar_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("time_ns", pa.int64(), False),
            ("end_time_ns", pa.int64(), False),
            ("symbol_sid", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("open", pa.int64(), False),
            ("high", pa.int64(), False),
            ("low", pa.int64(), False),
            ("close", pa.int64(), False),
            ("volume", pa.int64(), False),
            ("period_ns", pa.int64(), False),
        ]
    )


def quote_bar_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("time_ns", pa.int64(), False),
            ("end_time_ns", pa.int64(), False),
            ("symbol_sid", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("bid_open", pa.int64(), True),
            ("bid_high", pa.int64(), True),
            ("bid_low", pa.int64(), True),
            ("bid_close", pa.int64(), True),
            ("ask_open", pa.int64(), True),
            ("ask_high", pa.int64(), True),
            ("ask_low", pa.int64(), True),
            ("ask_close", pa.int64(), True),
            ("last_bid_size", pa.int64(), False),
            ("last_ask_size", pa.int64(), False),
            ("period_ns", pa.int64(), False),
        ]
    )


def tick_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("time_ns", pa.int64(), False),
            ("symbol_sid", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("tick_type", pa.uint8(), False),
            ("value", pa.int64(), False),
            ("quantity", pa.int64(), False),
            ("bid_price", pa.int64(), False),
            ("ask_price", pa.int64(), False),
            ("bid_size", pa.int64(), False),
            ("ask_size", pa.int64(), False),
            ("exchange", pa.string(), True),
            ("sale_condition", pa.string(), True),
            ("suspicious", pa.bool_(), False),
        ]
    )


def margin_interest_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("time_ns", pa.int64(), False),
            ("symbol_sid", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("interest_rate", pa.int64(), False),
        ]
    )


def perpetual_context_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("time_ns", pa.int64(), False),
            ("end_time_ns", pa.int64(), False),
            ("symbol_sid", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("funding", pa.int64(), False),
            ("open_interest", pa.int64(), False),
            ("prev_day_px", pa.int64(), False),
            ("day_ntl_vlm", pa.int64(), False),
            ("premium", pa.int64(), False),
            ("oracle_px", pa.int64(), False),
            ("mark_px", pa.int64(), False),
            ("mid_px", pa.int64(), False),
            ("impact_bid_px", pa.int64(), False),
            ("impact_ask_px", pa.int64(), False),
            ("period_ns", pa.int64(), False),
        ]
    )


def option_eod_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("date_ns", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("underlying", pa.string(), False),
            ("expiration_ns", pa.int64(), False),
            ("strike", pa.int64(), False),
            ("right", pa.string(), False),
            ("open", pa.int64(), False),
            ("high", pa.int64(), False),
            ("low", pa.int64(), False),
            ("close", pa.int64(), False),
            ("volume", pa.int64(), False),
            ("bid", pa.int64(), False),
            ("ask", pa.int64(), False),
            ("bid_size", pa.int64(), False),
            ("ask_size", pa.int64(), False),
        ]
    )


def option_universe_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("date_ns", pa.int64(), False),
            ("symbol_value", pa.string(), False),
            ("underlying", pa.string(), False),
            ("expiration_ns", pa.int64(), False),
            ("strike", pa.int64(), False),
            ("right", pa.string(), False),
        ]
    )


def custom_points_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("date_ns", pa.int64(), False),
            ("value", pa.float64(), False),
            ("fields_json", pa.string(), False),
            ("source_type", pa.string(), False),
            ("ticker", pa.string(), False),
            ("day", pa.date32(), False),
        ]
    )


def factor_file_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("date_ns", pa.int64(), False),
            ("price_factor", pa.float64(), False),
            ("split_factor", pa.float64(), False),
            ("reference_price", pa.float64(), False),
            ("market", pa.string(), False),
            ("ticker", pa.string(), False),
        ]
    )


def map_file_arrow_schema() -> pa.Schema:
    return pa.schema(
        [
            ("date_ns", pa.int64(), False),
            ("ticker", pa.string(), False),
            ("market", pa.string(), False),
            ("permtick", pa.string(), False),
        ]
    )


if __name__ == "__main__":
    main()
