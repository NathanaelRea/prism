create trigger fail_changed_selected_step_update
before update of selected_step_run_id on auto_run
when new.selected_step_run_id is not old.selected_step_run_id
begin
  select raise(fail, 'injected wait repair failure');
end
