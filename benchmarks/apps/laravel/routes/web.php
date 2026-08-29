<?php

use App\Http\Controllers\BenchController;
use App\Http\Middleware\BenchHeaderFive;
use App\Http\Middleware\BenchHeaderFour;
use App\Http\Middleware\BenchHeaderOne;
use App\Http\Middleware\BenchHeaderThree;
use App\Http\Middleware\BenchHeaderTwo;
use Illuminate\Support\Facades\Route;

/*
 * The eight endpoints of benchmarks/CONTRACT.md.
 *
 * These are registered by bootstrap/app.php with no middleware group at all —
 * not `web` (sessions, cookies, CSRF) and not `api` — because the contract asks
 * for a stack on `/middleware` and on nothing else. Putting the session and
 * cookie middleware in front of `/plaintext` would measure a stack the contract
 * does not ask any of the other apps to run.
 */

Route::get('/plaintext', [BenchController::class, 'plaintext']);

Route::get('/json', [BenchController::class, 'json']);

Route::get('/users/{id}/posts/{slug}', [BenchController::class, 'params']);

Route::middleware([
    BenchHeaderOne::class,
    BenchHeaderTwo::class,
    BenchHeaderThree::class,
    BenchHeaderFour::class,
    BenchHeaderFive::class,
])->get('/middleware', [BenchController::class, 'middleware']);

Route::get('/json-big', [BenchController::class, 'jsonBig']);

Route::get('/db/user/{id}', [BenchController::class, 'dbUser']);

Route::get('/db/posts', [BenchController::class, 'dbPosts']);

Route::get('/template', [BenchController::class, 'template']);
