update notification_outbox
set delivery_state = 'uncertain', superseded_unix_ms = ?, last_failure_category = ?
where id = ? and delivery_state = 'dispatching'
