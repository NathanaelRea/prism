select id as "id!", title as "title!", body as "body!"
from notification_outbox
where delivery_state = 'pending' and available_unix_ms <= ?
order by id limit 1
