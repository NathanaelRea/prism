-- Alpha hard cutover: generalized Workflow Runs are global and no repository-local history is
-- imported. Clear cyclic legacy references before dropping their owners.
update auto_run set selected_step_run_id = null;
update auto_step_run set plan_run_id = null;

drop table auto_output_line;
drop table auto_event;
drop table auto_step_run;
drop table auto_run;
drop table plan_output_line;
drop table plan_step_run;
drop table plan_run;
drop table workflow_execution;
drop table integration_lane;
drop table merge_intent;
