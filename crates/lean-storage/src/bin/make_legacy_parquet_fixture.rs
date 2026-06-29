use std::fs::File;
use std::path::PathBuf;

use anyhow::Result;
use chrono::{NaiveDate, TimeZone, Utc};
use clap::Parser;
use lean_core::{Market, NanosecondTimestamp, Symbol, TimeSpan};
use lean_data::{TradeBar, TradeBarData};
use lean_storage::convert;
use parquet::arrow::arrow_writer::ArrowWriter;
use rust_decimal_macros::dec;

#[derive(Parser)]
struct Args {
    output: PathBuf,
}

fn main() -> Result<()> {
    let args = Args::parse();
    let partition = args
        .output
        .join("equity")
        .join("usa")
        .join("daily")
        .join("trade")
        .join("date=2022-05-03");
    std::fs::create_dir_all(&partition)?;

    let market = Market::usa();
    let spy = Symbol::create_equity("SPY", &market);
    let day = NaiveDate::from_ymd_opt(2022, 5, 3).unwrap();
    let time =
        NanosecondTimestamp::from(Utc.from_utc_datetime(&day.and_hms_opt(16, 0, 0).unwrap()));
    let bars = vec![TradeBar::new(
        spy,
        time,
        TimeSpan::ONE_DAY,
        TradeBarData::new(dec!(100), dec!(101), dec!(99), dec!(100.5), dec!(12345)),
    )];
    let batch = convert::trade_bars_to_record_batch(&bars);
    let file = File::create(partition.join("data.parquet"))?;
    let mut writer = ArrowWriter::try_new(file, batch.schema(), None)?;
    writer.write(&batch)?;
    writer.close()?;
    Ok(())
}
