<!doctype html>
<html>
<head><title>{{ $title }}</title></head>
<body>
<h1>{{ $title }}</h1>
<table>
@foreach ($rows as $row)
<tr><td>{{ $row['id'] }}</td><td>{{ $row['name'] }}</td></tr>
@endforeach
</table>
</body>
</html>
