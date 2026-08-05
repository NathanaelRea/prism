update notification_outbox
set delivery_state = 'delivered', backend_accepted_unix_ms = ?, last_failure_category = null
where id = ? and delivery_state = 'dispatching'
