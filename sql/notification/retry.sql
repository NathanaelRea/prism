update notification_outbox
set delivery_state = 'pending', available_unix_ms = ?, last_failure_category = ?
where id = ? and delivery_state = 'dispatching'
