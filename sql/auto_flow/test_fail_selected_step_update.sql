create trigger fail_selected_step_update
before update of selected_step_run_id on auto_run
when new.selected_step_run_id is not null
begin
  select raise(fail, 'injected late write failure');
end
