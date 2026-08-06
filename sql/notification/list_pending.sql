select id as "id!", title as "title!", body as "body!"
from notification_outbox where delivery_state = 'pending' order by id
