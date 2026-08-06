create trigger fail_runtime_upsert
before update of generation on opencode_runtime
when old.branch = 'feature/first'
begin
  select raise(abort, 'forced runtime upsert failure');
end
