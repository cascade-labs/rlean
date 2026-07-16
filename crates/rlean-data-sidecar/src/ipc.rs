use std::io::Cursor;

use arrow_array::RecordBatch;
use arrow_ipc::reader::StreamReader;
use arrow_ipc::writer::StreamWriter;

/// Keep Arrow IPC bodies comfortably below the Flight service's 16 MiB
/// transport ceiling. The remaining headroom covers Flight and protobuf
/// metadata without requiring callers to estimate their encoded size.
pub const MAX_RECORD_BATCH_BODY_BYTES: usize = 8 * 1024 * 1024;

pub fn encode_record_batch(batch: &RecordBatch) -> anyhow::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    {
        let mut writer = StreamWriter::try_new(&mut bytes, &batch.schema())?;
        writer.write(batch)?;
        writer.finish()?;
    }
    Ok(bytes)
}

/// Encode a record batch into one or more independently decodable IPC bodies.
///
/// Splitting by rows preserves ordering and lets the existing query and live
/// streams deliver large canonical batches without changing the wire protocol.
pub fn encode_record_batch_chunks(batch: &RecordBatch) -> anyhow::Result<Vec<Vec<u8>>> {
    encode_record_batch_chunks_with_limit(batch, MAX_RECORD_BATCH_BODY_BYTES)
}

fn encode_record_batch_chunks_with_limit(
    batch: &RecordBatch,
    max_body_bytes: usize,
) -> anyhow::Result<Vec<Vec<u8>>> {
    anyhow::ensure!(
        max_body_bytes > 0,
        "record batch body limit must be positive"
    );

    let mut encoded = Vec::new();
    encode_record_batch_slice(batch, max_body_bytes, &mut encoded)?;
    Ok(encoded)
}

fn encode_record_batch_slice(
    batch: &RecordBatch,
    max_body_bytes: usize,
    encoded: &mut Vec<Vec<u8>>,
) -> anyhow::Result<()> {
    let body = encode_record_batch(batch)?;
    if body.len() <= max_body_bytes {
        encoded.push(body);
        return Ok(());
    }

    anyhow::ensure!(
        batch.num_rows() > 1,
        "one Arrow record encodes to {} bytes, exceeding the {} byte Flight body limit",
        body.len(),
        max_body_bytes
    );

    let left_rows = batch.num_rows() / 2;
    encode_record_batch_slice(&batch.slice(0, left_rows), max_body_bytes, encoded)?;
    encode_record_batch_slice(
        &batch.slice(left_rows, batch.num_rows() - left_rows),
        max_body_bytes,
        encoded,
    )?;
    Ok(())
}

pub fn decode_record_batch(bytes: &[u8]) -> anyhow::Result<RecordBatch> {
    let mut reader = StreamReader::try_new(Cursor::new(bytes), None)?;
    reader
        .next()
        .transpose()?
        .ok_or_else(|| anyhow::anyhow!("Flight data message contained no Arrow record batch"))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use arrow_array::{Array, RecordBatch, StringArray};
    use arrow_schema::{DataType, Field, Schema};

    use super::{decode_record_batch, encode_record_batch_chunks_with_limit};

    fn string_batch(values: Vec<String>) -> RecordBatch {
        RecordBatch::try_new(
            Arc::new(Schema::new(vec![Field::new(
                "fields_json",
                DataType::Utf8,
                false,
            )])),
            vec![Arc::new(StringArray::from(values))],
        )
        .unwrap()
    }

    #[test]
    fn chunks_large_batches_into_independently_decodable_ordered_bodies() {
        let values = (0..20)
            .map(|index| format!("{index:02}-{}", "x".repeat(700_000)))
            .collect::<Vec<_>>();
        let batch = string_batch(values.clone());

        let chunks = encode_record_batch_chunks_with_limit(&batch, 2 * 1024 * 1024).unwrap();

        assert!(chunks.len() > 1);
        assert!(chunks.iter().all(|chunk| chunk.len() <= 2 * 1024 * 1024));
        let decoded = chunks
            .iter()
            .map(|chunk| decode_record_batch(chunk).unwrap())
            .collect::<Vec<_>>();
        let actual = decoded
            .iter()
            .flat_map(|batch| {
                let values = batch
                    .column(0)
                    .as_any()
                    .downcast_ref::<StringArray>()
                    .unwrap();
                (0..values.len()).map(|index| values.value(index).to_owned())
            })
            .collect::<Vec<_>>();
        assert_eq!(actual, values);
    }

    #[test]
    fn rejects_one_record_that_cannot_fit() {
        let batch = string_batch(vec!["x".repeat(2048)]);

        let error = encode_record_batch_chunks_with_limit(&batch, 1024).unwrap_err();

        assert!(error.to_string().contains("one Arrow record encodes"));
    }
}
