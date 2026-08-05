create trigger reject_policy_refresh before update on repo_policy_cache
begin
  select raise(abort, 'policy refresh rejected');
end
