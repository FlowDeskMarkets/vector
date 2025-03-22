Adds batch insert optimization for BigQuery sink through the new `batch_timeout_secs` option. 

This allows you to specify a timeout for batches to fill up before being sent, balancing between throughput and latency. With a longer timeout, fewer API calls are made with larger batches, which can significantly reduce costs and improve performance for high-volume data ingestion.