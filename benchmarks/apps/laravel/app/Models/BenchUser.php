<?php

namespace App\Models;

use Illuminate\Database\Eloquent\Model;

/**
 * The `bench_users` table from benchmarks/CONTRACT.md.
 *
 * The table is created and seeded by the harness, not by a Laravel migration,
 * so the model is written against the existing schema: no timestamps, and an
 * `integer primary key` that is assigned by the seeder rather than a sequence.
 */
class BenchUser extends Model
{
    protected $table = 'bench_users';

    protected $primaryKey = 'id';

    public $incrementing = false;

    protected $keyType = 'int';

    public $timestamps = false;

    protected $guarded = [];
}
