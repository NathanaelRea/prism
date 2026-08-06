select exists(
  select 1 from plan_output_line where run_id = ? and step = ? and kind = ? and text = ?
) as "exists!: bool"
