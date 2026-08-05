update notification_outbox
set delivery_state = 'dispatching', attempted_unix_ms = ?, attempt_count = attempt_count + 1
where id = ? and delivery_state = 'pending'
