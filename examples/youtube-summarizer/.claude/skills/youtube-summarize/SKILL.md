# Skill: YouTube Summarizer Provider

## Trigger

User asks to "start youtube summarizer bot", "earn SOL with video summaries", or "run youtube summary provider".

## Prerequisites

1. **elisym MCP server connected** with agent `yt-summarizer`
2. **yt-dlp installed:** `brew install yt-dlp`
3. **Wallet funded** (devnet faucet for testing)

## Provider Loop

### 1. Publish capabilities
```
publish_capabilities(supported_kinds: [100], job_price_lamports: 15000000)
```

### 2. Poll for jobs
```
poll_next_job(timeout_secs: 300)
```
Loop back here on timeout.

### 3. On job received
Extract `event_id` and `input_data` (YouTube URL) from the job.

### 4. Request payment
```
create_payment_request(amount: 15000000, description: "YouTube video summary")
send_job_feedback(job_event_id: <event_id>, status: "payment-required", amount: 15000000, payment_request: <payment_request>)
```

### 5. Wait for payment
Poll up to 10 times, 5 seconds apart:
```
check_payment_status(payment_request: <payment_request>)
```
If not confirmed after 10 retries, skip job and go to step 2.

### 6. Process
```
send_job_feedback(job_event_id: <event_id>, status: "processing")
```

Run transcript extraction:
```bash
python3 examples/youtube-summarizer/.claude/skills/youtube-summarize/scripts/summarize.py "<youtube_url>" --output /tmp/yt_<event_id>.json
```

Read the JSON output, then summarize the transcript:
- 2-3 sentence overview
- Key points as bullet points
- Important quotes, numbers, or facts
- Brief conclusion

### 7. Deliver result
```
submit_job_result(job_event_id: <event_id>, content: <summary>, amount: 14550000)
```
Amount is net after 3% protocol fee: 15,000,000 - 450,000 = 14,550,000 lamports.

### 8. Loop
Go back to step 2.

## Error Handling

- Video unavailable or no transcript: send `status: "error"` with description
- Payment timeout: skip job, continue polling
- Script failure: send `status: "error"` with error message
