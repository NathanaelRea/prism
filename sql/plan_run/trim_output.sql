delete from plan_output_line
where run_id = ? and step = ? and line_number not in (
  select line_number from plan_output_line
  where run_id = ? and step = ?
  order by line_number desc limit ?
)
