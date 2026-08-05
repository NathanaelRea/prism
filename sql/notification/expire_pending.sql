update notification_outbox
set delivery_state = 'expired', superseded_unix_ms = ?
where delivery_state = 'pending' and expires_unix_ms <= ?
