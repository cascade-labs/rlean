use std::io::Cursor;

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;

pub fn encode_record_batch(batch: &RecordBatch) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &batch.schema())?;
        writer.write(batch)?;
        writer.finish()?;
    }
    Ok(bytes)
}

pub fn decode_record_batch(bytes: &[u8]) -> anyhow::Result<RecordBatch> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None)?;
    reader
        .next()
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("Flight data message contained no Arrow record batch"))
}
