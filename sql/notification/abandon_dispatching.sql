update notification_outbox
set delivery_state = 'uncertain', superseded_unix_ms = ?, last_failure_category = 'interrupted_dispatch'
where delivery_state = 'dispatching'
