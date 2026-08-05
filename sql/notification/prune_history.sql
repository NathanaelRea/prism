delete from notification_outbox
where delivery_state not in ('pending', 'dispatching') and observed_unix_ms < ?
