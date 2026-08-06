delete from auto_output_line
where step_run_id = ?
  and line_number not in (
    select line_number
    from auto_output_line
    where step_run_id = ?
    order by line_number desc
    limit ?
  )
