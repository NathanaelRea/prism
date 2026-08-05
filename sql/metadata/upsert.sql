insert into metadata (key, value)
values (?1, ?2)
on conflict(key) do update set value = excluded.value
